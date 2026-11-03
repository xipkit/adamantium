import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const labels = ["violation", "no_violation", "insufficient_context"];

/** Validate the rule set and its labeled examples before making API requests. */
export function validateInputs(rules, cases) {
  if (typeof rules.model !== "string" || !rules.model || !Array.isArray(rules.questions)) {
    throw new Error("The rule set must contain a model and a questions array.");
  }
  if (rules.questions.length < 1 || rules.questions.length > 400) {
    throw new Error("The rule set must contain between 1 and 400 questions.");
  }

  const questions = new Map();
  for (const question of rules.questions) {
    if (question.type !== "choice" || typeof question.name !== "string" || !question.name ||
        typeof question.instructions !== "string" || !question.instructions ||
        !Array.isArray(question.choices)) {
      throw new Error("Every rule must be a named choice question with instructions.");
    }
    const values = question.choices.map((choice) => choice.value);
    if (values.length !== labels.length || labels.some((label) => !values.includes(label))) {
      throw new Error(`Rule ${question.name} must contain exactly the three review choices.`);
    }
    if (questions.has(question.name)) throw new Error(`Duplicate rule: ${question.name}`);
    questions.set(question.name, question);
  }

  if (!Array.isArray(cases) || cases.length === 0) throw new Error("No examples were supplied.");
  const caseNames = new Set();
  for (const example of cases) {
    if (typeof example.name !== "string" || !example.name || caseNames.has(example.name) ||
        !questions.has(example.rule) || !labels.includes(example.expected) ||
        typeof example.source !== "string" || !example.source.trim() ||
        (example.context !== undefined && typeof example.context !== "string")) {
      throw new Error(`Invalid example: ${example.name ?? "unnamed"}`);
    }
    caseNames.add(example.name);
  }
  for (const name of questions.keys()) {
    const outcomes = new Set(cases.filter((example) => example.rule === name).map((example) => example.expected));
    if (!outcomes.has("violation") || !outcomes.has("no_violation")) {
      throw new Error(`Rule ${name} needs both a positive and a negative example.`);
    }
  }
  return questions;
}

/** Construct a Decisions request for one labeled candidate. */
export function buildRequest(model, question, source, context = "") {
  const input = context ? `Project context:\n${context}\n\nRust candidate:\n${source}` : source;
  return { model, input, questions: [question] };
}

/** Validate and extract a choice or refusal from the API response. */
export function parseAnswer(response, name) {
  if (!Array.isArray(response.answers) || response.answers.length !== 1) {
    throw new Error("The API did not return exactly one answer.");
  }
  const answer = response.answers[0];
  if (answer.name !== name) throw new Error("The API returned an answer for the wrong rule.");
  if (answer.type === "refusal") return { type: "refusal" };
  const probabilityIsValid = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
  if (answer.type !== "choice" || !labels.includes(answer.choice) ||
      !probabilityIsValid(answer.confidence) || !Array.isArray(answer.probabilities)) {
    throw new Error("The API returned an invalid choice answer.");
  }
  const probabilities = new Map();
  for (const entry of answer.probabilities) {
    if (!labels.includes(entry.value) || probabilities.has(entry.value) || !probabilityIsValid(entry.probability)) {
      throw new Error("The API returned an invalid probability distribution.");
    }
    probabilities.set(entry.value, entry.probability);
  }
  if (probabilities.size !== labels.length) throw new Error("The API omitted a choice probability.");
  return { type: "choice", choice: answer.choice, confidence: answer.confidence, probabilities };
}

function readApiKey() {
  if (process.env.OPENAI_API_KEY?.trim()) return process.env.OPENAI_API_KEY.trim();
  try {
    const key = execFileSync("security", ["find-generic-password", "-s", "openai-api-key", "-w"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 10000,
    }).trim();
    if (key) return key;
  } catch {
    throw new Error("Could not read the openai-api-key item from Keychain.");
  }
  throw new Error("The openai-api-key Keychain item is empty.");
}

/** Send a request directly to the Decisions endpoint without logging the key. */
export async function requestDecision(apiKey, request) {
  const response = await fetch("https://api.openai.com/v1/decisions", {
    method: "POST",
    headers: { Authorization: `Bearer ${apiKey}`, "Content-Type": "application/json" },
    body: JSON.stringify(request),
    redirect: "error",
    signal: AbortSignal.timeout(30000),
  });
  if (!response.ok) throw new Error(`Decisions API returned HTTP ${response.status}.`);
  return response.json();
}

async function main() {
  const options = process.argv.slice(2);
  let dryRun = false;
  let ruleName;
  for (let index = 0; index < options.length; index += 1) {
    if (options[index] === "--dry-run" && !dryRun) {
      dryRun = true;
    } else if (options[index] === "--rule" && ruleName === undefined && options[index + 1] &&
               !options[index + 1].startsWith("--")) {
      ruleName = options[index + 1];
      index += 1;
    } else {
      throw new Error("Usage: node scripts/evaluate-decisions.mjs [--dry-run] [--rule NAME]");
    }
  }
  const rules = JSON.parse(await readFile(path.join(root, "rules/google-rust-decisions.json"), "utf8"));
  const cases = JSON.parse(await readFile(path.join(root, "fixtures/google-rust-cases.json"), "utf8"));
  const questions = validateInputs(rules, cases);
  if (ruleName && !questions.has(ruleName)) throw new Error(`Unknown rule: ${ruleName}`);
  const selected = ruleName ? cases.filter((example) => example.rule === ruleName) : cases;
  if (dryRun) {
    for (const example of selected) {
      JSON.stringify(buildRequest(rules.model, questions.get(example.rule), example.source, example.context));
    }
    console.log(`Validated ${questions.size} rules and ${cases.length} labeled examples; ` +
      `${selected.length} selected without making API requests.`);
    return;
  }

  const apiKey = readApiKey();
  let matches = 0;
  let inputTokens = 0;
  for (const example of selected) {
    const request = buildRequest(rules.model, questions.get(example.rule), example.source, example.context);
    const response = await requestDecision(apiKey, request);
    const answer = parseAnswer(response, example.rule);
    inputTokens += response.usage?.input_tokens ?? 0;
    if (answer.type === "refusal") {
      console.log(`${example.name}: refusal (expected ${example.expected})`);
      continue;
    }
    if (answer.choice === example.expected) matches += 1;
    console.log(`${example.name}: ${answer.choice} (expected ${example.expected}; ` +
      `P(violation)=${answer.probabilities.get("violation").toFixed(3)}, confidence=${answer.confidence.toFixed(3)})`);
  }
  console.log(`Matched ${matches}/${selected.length} labeled examples; ${inputTokens} input tokens.`);
  if (matches !== selected.length) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main().catch((error) => {
    const code = error.cause?.code;
    console.error(code ? `${error.message} (${code})` : error.message);
    process.exitCode = 1;
  });
}
