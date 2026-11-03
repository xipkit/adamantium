import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { buildRequest, parseAnswer, requestDecision, validateInputs } from "./evaluate-decisions.mjs";

const rules = JSON.parse(await readFile(new URL("../rules/google-rust-decisions.json", import.meta.url)));
const cases = JSON.parse(await readFile(new URL("../fixtures/google-rust-cases.json", import.meta.url)));

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
