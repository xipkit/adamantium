import { execFileSync } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
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

/** Compute precision, recall, exact accuracy, pair discrimination, and slice metrics. */
export function summarizeResults(records) {
  const computeBucket = (items) => {
    let exactMatches = 0;
    let refusals = 0;
    let tp = 0;
    let fp = 0;
    let fpOnSafe = 0;
    let fpOnInsufficient = 0;
    let fn = 0;
    let fnAsNoViolation = 0;
    let fnAsInsufficient = 0;
    let fnAsRefusal = 0;
    let tn = 0;
    const confusion = {};
    const slices = {};
    const pairMap = new Map();
    const mistakes = [];

    for (const item of items) {
      if (item.predicted === item.expected) exactMatches += 1;
      if (item.predicted === "refusal") refusals += 1;

      if (!confusion[item.expected]) confusion[item.expected] = {};
      confusion[item.expected][item.predicted] = (confusion[item.expected][item.predicted] ?? 0) + 1;

      const expectedPositive = item.expected === "violation";
      const predictedPositive = item.predicted === "violation";
      if (expectedPositive && predictedPositive) {
        tp += 1;
      } else if (!expectedPositive && predictedPositive) {
        fp += 1;
        if (item.expected === "no_violation") fpOnSafe += 1;
        else if (item.expected === "insufficient_context") fpOnInsufficient += 1;
      } else if (expectedPositive && !predictedPositive) {
        fn += 1;
        if (item.predicted === "no_violation") fnAsNoViolation += 1;
        else if (item.predicted === "insufficient_context") fnAsInsufficient += 1;
        else if (item.predicted === "refusal") fnAsRefusal += 1;
      } else {
        tn += 1;
      }

      const sliceName = item.slice ?? "unspecified";
      if (!slices[sliceName]) {
        slices[sliceName] = { total: 0, exactMatches: 0, predictedViolation: 0, tp: 0, fp: 0, fn: 0 };
      }
      const sliceStats = slices[sliceName];
      sliceStats.total += 1;
      if (item.predicted === item.expected) sliceStats.exactMatches += 1;
      if (predictedPositive) sliceStats.predictedViolation += 1;
      if (expectedPositive && predictedPositive) sliceStats.tp += 1;
      else if (!expectedPositive && predictedPositive) sliceStats.fp += 1;
      else if (expectedPositive && !predictedPositive) sliceStats.fn += 1;

      if (item.pair) {
        const pairKey = `${item.rule}::${item.pair}`;
        if (!pairMap.has(pairKey)) pairMap.set(pairKey, {});
        const entry = pairMap.get(pairKey);
        if (item.expected === "violation") entry.violation = item;
        else if (item.expected === "no_violation") entry.safe = item;
      }

      if (item.predicted !== item.expected) {
        mistakes.push({
          name: item.name,
          rule: item.rule,
          expected: item.expected,
          predicted: item.predicted,
          slice: item.slice ?? null,
          pair: item.pair ?? null,
          existing_lint: item.existing_lint ?? null,
          pViolation: item.pViolation ?? null,
          confidence: item.confidence ?? null,
        });
      }
    }

    const pairs = { total: 0, bothCorrect: 0, violationMissed: 0, safeFalsePositive: 0, safeInsufficient: 0 };
    for (const pair of pairMap.values()) {
      if (!pair.violation || !pair.safe) continue;
      pairs.total += 1;
      const vOk = pair.violation.predicted === "violation";
      const sOk = pair.safe.predicted === "no_violation";
      if (vOk && sOk) pairs.bothCorrect += 1;
      if (!vOk) pairs.violationMissed += 1;
      if (pair.safe.predicted === "violation") pairs.safeFalsePositive += 1;
      if (pair.safe.predicted === "insufficient_context") pairs.safeInsufficient += 1;
    }

    const precision = tp + fp > 0 ? tp / (tp + fp) : null;
    const recall = tp + fn > 0 ? tp / (tp + fn) : null;
    const f1 = precision !== null && recall !== null && precision + recall > 0
      ? (2 * precision * recall) / (precision + recall)
      : null;

    return {
      total: items.length,
      exactMatches,
      exactAccuracy: items.length > 0 ? exactMatches / items.length : null,
      refusals,
      tp,
      fp,
      fpOnSafe,
      fpOnInsufficient,
      fn,
      fnAsNoViolation,
      fnAsInsufficient,
      fnAsRefusal,
      tn,
      precision,
      recall,
      f1,
      confusion,
      pairs,
      slices,
      mistakes,
    };
  };

  const byRule = {};
  const ruleOrder = [];
  for (const record of records) {
    if (!byRule[record.rule]) {
      byRule[record.rule] = [];
      ruleOrder.push(record.rule);
    }
    byRule[record.rule].push(record);
  }
  const perRule = {};
  for (const rule of ruleOrder) {
    perRule[rule] = computeBucket(byRule[rule]);
  }
  return { overall: computeBucket(records), perRule };
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

function formatMetric(value) {
  return value === null ? "n/a" : `${(value * 100).toFixed(1)}%`;
}

function parseCliArgs(options) {
  const config = {
    dryRun: false,
    metrics: false,
    ruleName: undefined,
    rulesPath: undefined,
    casesPath: undefined,
    splitName: undefined,
    outputPath: undefined,
  };
  const takeValue = (index, flag) => {
    const value = options[index + 1];
    if (!value || value.startsWith("--")) {
      throw new Error(`Missing value for ${flag}`);
    }
    return value;
  };
  for (let index = 0; index < options.length; index += 1) {
    const option = options[index];
    if (option === "--dry-run" && !config.dryRun) {
      config.dryRun = true;
    } else if (option === "--metrics" && !config.metrics) {
      config.metrics = true;
    } else if (option === "--rule" && config.ruleName === undefined) {
      config.ruleName = takeValue(index, option);
      index += 1;
    } else if (option === "--rules" && config.rulesPath === undefined) {
      config.rulesPath = takeValue(index, option);
      index += 1;
    } else if (option === "--cases" && config.casesPath === undefined) {
      config.casesPath = takeValue(index, option);
      index += 1;
    } else if (option === "--split" && config.splitName === undefined) {
      config.splitName = takeValue(index, option);
      index += 1;
    } else if (option === "--output" && config.outputPath === undefined) {
      config.outputPath = takeValue(index, option);
      index += 1;
    } else {
      throw new Error(
        "Usage: node scripts/evaluate-decisions.mjs [--dry-run] [--metrics] [--rule NAME] " +
          "[--rules PATH] [--cases PATH] [--split NAME] [--output PATH]",
      );
    }
  }
  return config;
}

async function main() {
  const config = parseCliArgs(process.argv.slice(2));
  const rulesFile = config.rulesPath
    ? path.resolve(config.rulesPath)
    : path.join(root, "../cli/src/style/google-rust-decisions.json");
  const casesFile = config.casesPath
    ? path.resolve(config.casesPath)
    : path.join(root, "fixtures/google-rust-cases.json");
  const rules = JSON.parse(await readFile(rulesFile, "utf8"));
  const cases = JSON.parse(await readFile(casesFile, "utf8"));
  const questions = validateInputs(rules, cases);
  if (config.ruleName && !questions.has(config.ruleName)) throw new Error(`Unknown rule: ${config.ruleName}`);
  let selected = config.ruleName ? cases.filter((example) => example.rule === config.ruleName) : cases;
  if (config.splitName !== undefined) {
    selected = selected.filter((example) => example.split === config.splitName);
    if (selected.length === 0) throw new Error(`No examples matched split: ${config.splitName}`);
  }
  if (config.dryRun) {
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
  let requestsSent = 0;
  const records = [];
  for (const example of selected) {
    const request = buildRequest(rules.model, questions.get(example.rule), example.source, example.context);
    requestsSent += 1;
    const response = await requestDecision(apiKey, request);
    const answer = parseAnswer(response, example.rule);
    const tokens = response.usage?.input_tokens ?? 0;
    inputTokens += tokens;
    if (answer.type === "refusal") {
      console.log(`${example.name}: refusal (expected ${example.expected})`);
      records.push({
        name: example.name,
        rule: example.rule,
        split: example.split ?? null,
        slice: example.slice ?? null,
        pair: example.pair ?? null,
        existing_lint: example.existing_lint ?? null,
        expected: example.expected,
        predicted: "refusal",
        pViolation: null,
        confidence: null,
        probabilities: null,
        inputTokens: tokens,
      });
      continue;
    }
    if (answer.choice === example.expected) matches += 1;
    const pViolation = answer.probabilities.get("violation");
    records.push({
      name: example.name,
      rule: example.rule,
      split: example.split ?? null,
      slice: example.slice ?? null,
      pair: example.pair ?? null,
      existing_lint: example.existing_lint ?? null,
      expected: example.expected,
      predicted: answer.choice,
      pViolation,
      confidence: answer.confidence,
      probabilities: Object.fromEntries(answer.probabilities),
      inputTokens: tokens,
    });
    console.log(`${example.name}: ${answer.choice} (expected ${example.expected}; ` +
      `P(violation)=${pViolation.toFixed(3)}, confidence=${answer.confidence.toFixed(3)})`);
  }
  console.log(`Matched ${matches}/${selected.length} labeled examples; ${requestsSent} requests; ${inputTokens} input tokens.`);
  const summary = summarizeResults(records);
  if (config.metrics) {
    console.log("\nPer-rule metrics (violation = positive):");
    for (const [rule, stats] of Object.entries(summary.perRule)) {
      console.log(
        `  ${rule}: precision=${formatMetric(stats.precision)} (${stats.tp}/${stats.tp + stats.fp}), ` +
          `recall=${formatMetric(stats.recall)} (${stats.tp}/${stats.tp + stats.fn}), ` +
          `F1=${formatMetric(stats.f1)}, exact=${stats.exactMatches}/${stats.total}, ` +
          `pairs=${stats.pairs.bothCorrect}/${stats.pairs.total}`,
      );
    }
    const overall = summary.overall;
    console.log(
      `  OVERALL: precision=${formatMetric(overall.precision)} (${overall.tp}/${overall.tp + overall.fp}), ` +
        `recall=${formatMetric(overall.recall)} (${overall.tp}/${overall.tp + overall.fn}), ` +
        `F1=${formatMetric(overall.f1)}, exact=${overall.exactMatches}/${overall.total}, ` +
        `pairs=${overall.pairs.bothCorrect}/${overall.pairs.total}`,
    );
  }
  if (config.outputPath) {
    const payload = {
      timestamp: new Date().toISOString(),
      model: rules.model,
      rulesPath: path.relative(process.cwd(), rulesFile),
      casesPath: path.relative(process.cwd(), casesFile),
      split: config.splitName ?? null,
      ruleFilter: config.ruleName ?? null,
      requestsSent,
      inputTokens,
      summary,
      results: records,
    };
    await writeFile(path.resolve(config.outputPath), `${JSON.stringify(payload, null, 2)}\n`, "utf8");
  }
  if (!config.metrics && matches !== selected.length) process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main().catch((error) => {
    const code = error.cause?.code;
    console.error(code ? `${error.message} (${code})` : error.message);
    process.exitCode = 1;
  });
}
