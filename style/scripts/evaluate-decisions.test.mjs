import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { buildRequest, parseAnswer, requestDecision, summarizeResults, validateInputs } from "./evaluate-decisions.mjs";

const rules = JSON.parse(await readFile(new URL("../../cli/src/style/google-rust-decisions.json", import.meta.url)));
const cases = JSON.parse(await readFile(new URL("../fixtures/google-rust-cases.json", import.meta.url)));
const concurrencyRules = JSON.parse(await readFile(new URL("../rules/concurrency-decisions.json", import.meta.url)));
const concurrencyCases = JSON.parse(await readFile(new URL("../fixtures/concurrency-cases.json", import.meta.url)));

test("each rule has labeled positive and negative examples", () => {
  const questions = validateInputs(rules, cases);
  assert.equal(questions.size, rules.questions.length);
  const example = cases[0];
  const request = buildRequest(rules.model, questions.get(example.rule), example.source);
  assert.deepEqual(Object.keys(request).sort(), ["input", "model", "questions"]);
  assert.equal(request.input, example.source);
  assert.equal(request.questions[0].name, example.rule);
  assert.equal(Object.hasOwn(request, "expected"), false);
  assert.equal(JSON.stringify(request).includes(example.name), false);
});

test("the coverage map includes every rule", async () => {
  const coverage = await readFile(new URL("../rules/coverage.md", import.meta.url), "utf8");
  for (const question of rules.questions) {
    assert.ok(coverage.includes(`\`${question.name}\``), `Missing coverage for ${question.name}`);
  }
});

test("context is included as evidence without leaking the expected answer", () => {
  const example = cases.find((item) => item.context);
  assert.ok(example);
  const question = rules.questions.find((item) => item.name === example.rule);
  const request = buildRequest(rules.model, question, example.source, example.context);
  assert.ok(request.input.includes(example.context));
  assert.ok(request.input.includes(example.source));
  assert.equal(Object.hasOwn(request, "expected"), false);
});

test("a dry run can select examples for one rule without contacting the API", () => {
  const script = fileURLToPath(new URL("./evaluate-decisions.mjs", import.meta.url));
  const output = execFileSync(process.execPath, [script, "--dry-run", "--rule", "glob_import_context"], { encoding: "utf8" });
  assert.match(output, /3 selected without making API requests/);
});

test("choice answers preserve the selected label and violation probability", () => {
  const response = {
    answers: [{
      type: "choice",
      name: "unnecessary_shared_ownership",
      choice: "violation",
      confidence: 0.8,
      probabilities: [
        { value: "violation", probability: 0.8 },
        { value: "no_violation", probability: 0.1 },
        { value: "insufficient_context", probability: 0.1 },
      ],
    }],
  };
  const answer = parseAnswer(response, "unnecessary_shared_ownership");
  assert.equal(answer.choice, "violation");
  assert.equal(answer.probabilities.get("violation"), 0.8);
  assert.throws(() => parseAnswer(response, "another_rule"), /wrong rule/);
  response.answers[0].probabilities.pop();
  assert.throws(() => parseAnswer(response, "unnecessary_shared_ownership"), /omitted/);
});

test("refusals remain unresolved instead of being treated as clean results", () => {
  assert.deepEqual(parseAnswer({ answers: [{ type: "refusal", name: "a_rule" }] }, "a_rule"), { type: "refusal" });
});

test("the request uses the Decisions endpoint and sends the key only as a header", async (t) => {
  const payload = buildRequest(rules.model, rules.questions[0], "fn sample() {}");
  const result = { answers: [{ type: "refusal", name: rules.questions[0].name }] };
  const mock = t.mock.method(globalThis, "fetch", async (url, options) => {
    assert.equal(url, "https://api.openai.com/v1/decisions");
    assert.equal(options.method, "POST");
    assert.equal(options.redirect, "error");
    assert.equal(options.headers.Authorization, "Bearer local-test-key");
    assert.deepEqual(JSON.parse(options.body), payload);
    assert.equal(options.body.includes("local-test-key"), false);
    return { ok: true, json: async () => result };
  });
  assert.deepEqual(await requestDecision("local-test-key", payload), result);
  assert.equal(mock.mock.callCount(), 1);
});

test("a failed API request reports its status without including the response body", async (t) => {
  t.mock.method(globalThis, "fetch", async () => ({ ok: false, status: 403 }));
  await assert.rejects(requestDecision("local-test-key", {}), /^Error: Decisions API returned HTTP 403\.$/);
});

test("concurrency corpus has valid paired cases, splits, and existing-lint tags", () => {
  const questions = validateInputs(concurrencyRules, concurrencyCases);
  assert.equal(questions.size, 4);

  const validSplits = new Set(["dev", "held_out"]);
  const validSlices = new Set(["semantic_only", "lint_overlap", "lint_decoy", "insufficient_context"]);
  const pairs = new Map();

  for (const example of concurrencyCases) {
    assert.ok(validSplits.has(example.split), `Invalid split on ${example.name}`);
    assert.ok(validSlices.has(example.slice), `Invalid slice on ${example.name}`);
    if (example.slice === "lint_overlap" || example.slice === "lint_decoy") {
      assert.equal(typeof example.existing_lint, "string", `Missing existing_lint on ${example.name}`);
      assert.ok(example.existing_lint.trim().length > 0, `Empty existing_lint on ${example.name}`);
    } else {
      assert.equal(example.existing_lint, null, `Unexpected existing_lint on ${example.name}`);
    }
    const request = buildRequest(concurrencyRules.model, questions.get(example.rule), example.source, example.context);
    assert.equal(JSON.stringify(request).includes(example.name), false, `Case name leaked in ${example.name}`);

    if (example.pair !== null) {
      const key = `${example.rule}::${example.split}::${example.pair}`;
      if (!pairs.has(key)) pairs.set(key, []);
      pairs.get(key).push(example);
    }
  }

  for (const [key, group] of pairs.entries()) {
    assert.equal(group.length, 2, `Pair ${key} must have exactly two cases`);
    const outcomes = new Set(group.map((item) => item.expected));
    assert.deepEqual(outcomes, new Set(["violation", "no_violation"]), `Pair ${key} must pair violation and no_violation`);
  }

  for (const ruleName of questions.keys()) {
    const heldOut = concurrencyCases.filter((item) => item.rule === ruleName && item.split === "held_out");
    const slices = new Set(heldOut.map((item) => item.slice));
    assert.deepEqual(slices, validSlices, `Rule ${ruleName} held_out must cover all four slices`);
  }
});

test("the concurrency evaluation report covers every candidate rule", async () => {
  const report = await readFile(new URL("../rules/concurrency-evaluation.md", import.meta.url), "utf8");
  for (const question of concurrencyRules.questions) {
    assert.ok(report.includes(`\`${question.name}\``), `Missing report coverage for ${question.name}`);
  }
});

test("a dry run can select a split of the concurrency corpus without contacting the API", () => {
  const script = fileURLToPath(new URL("./evaluate-decisions.mjs", import.meta.url));
  const rulesPath = fileURLToPath(new URL("../rules/concurrency-decisions.json", import.meta.url));
  const casesPath = fileURLToPath(new URL("../fixtures/concurrency-cases.json", import.meta.url));
  const output = execFileSync(
    process.execPath,
    [script, "--dry-run", "--rules", rulesPath, "--cases", casesPath, "--split", "held_out"],
    { encoding: "utf8" },
  );
  assert.match(output, /Validated 4 rules and 104 labeled examples; 88 selected without making API requests/);
});

test("summarizeResults computes precision, recall, pair discrimination, and slice metrics", () => {
  const summary = summarizeResults([
    { name: "p1_bug", rule: "r1", expected: "violation", predicted: "violation", slice: "semantic_only", pair: "p1", existing_lint: null, pViolation: 0.9, confidence: 0.9 },
    { name: "p1_safe", rule: "r1", expected: "no_violation", predicted: "no_violation", slice: "semantic_only", pair: "p1", existing_lint: null, pViolation: 0.1, confidence: 0.9 },
    { name: "p2_bug", rule: "r1", expected: "violation", predicted: "no_violation", slice: "semantic_only", pair: "p2", existing_lint: null, pViolation: 0.2, confidence: 0.8 },
    { name: "p2_safe", rule: "r1", expected: "no_violation", predicted: "violation", slice: "semantic_only", pair: "p2", existing_lint: null, pViolation: 0.7, confidence: 0.7 },
    { name: "lint_hit", rule: "r1", expected: "violation", predicted: "violation", slice: "lint_overlap", pair: null, existing_lint: "clippy::disallowed_types", pViolation: 0.95, confidence: 0.95 },
    { name: "decoy", rule: "r1", expected: "no_violation", predicted: "no_violation", slice: "lint_decoy", pair: null, existing_lint: "clippy::disallowed_methods", pViolation: 0.05, confidence: 0.95 },
    { name: "insuff", rule: "r1", expected: "insufficient_context", predicted: "violation", slice: "insufficient_context", pair: null, existing_lint: null, pViolation: 0.6, confidence: 0.6 },
    { name: "refused", rule: "r1", expected: "violation", predicted: "refusal", slice: "semantic_only", pair: null, existing_lint: null, pViolation: null, confidence: null },
  ]);

  const r1 = summary.perRule.r1;
  assert.equal(r1.tp, 2);
  assert.equal(r1.fp, 2);
  assert.equal(r1.fpOnSafe, 1);
  assert.equal(r1.fpOnInsufficient, 1);
  assert.equal(r1.fn, 2);
  assert.equal(r1.fnAsNoViolation, 1);
  assert.equal(r1.fnAsRefusal, 1);
  assert.equal(r1.refusals, 1);
  assert.equal(r1.precision, 0.5);
  assert.equal(r1.recall, 0.5);
  assert.equal(r1.f1, 0.5);
  assert.equal(r1.pairs.total, 2);
  assert.equal(r1.pairs.bothCorrect, 1);
  assert.equal(r1.pairs.violationMissed, 1);
  assert.equal(r1.pairs.safeFalsePositive, 1);
  assert.equal(r1.slices.lint_overlap.tp, 1);
  assert.equal(r1.slices.lint_decoy.fp, 0);
  assert.equal(r1.mistakes.length, 4);
});
