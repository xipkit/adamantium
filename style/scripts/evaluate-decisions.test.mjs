import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import {
  buildCliArgs,
  evaluateCaseWithCli,
  getCaseLabels,
  parseCliReport,
  summarizeResults,
  validateCasePath,
  validateCases,
} from "./evaluate-decisions.mjs";

const googleRules = JSON.parse(await readFile(new URL("../../cli/src/style/google-rust-decisions.json", import.meta.url)));
const googleCases = JSON.parse(await readFile(new URL("../fixtures/google-rust-cases.json", import.meta.url)));
const concurrencyRules = JSON.parse(await readFile(new URL("../rules/concurrency-decisions.json", import.meta.url)));
const concurrencyCases = JSON.parse(await readFile(new URL("../fixtures/concurrency-cases.json", import.meta.url)));

const packCandidateRules = [
  "split_actor_invariant",
  "stale_read_across_await",
  "unbounded_collection_growth",
  "partial_mutation_on_cancellation",
];

test("google corpus covers every embedded google rule with positive and negative cases", () => {
  const labeledRules = validateCases(googleCases);
  for (const question of googleRules.questions) {
    assert.ok(labeledRules.has(question.name), `Missing examples for ${question.name}`);
  }
});

test("the coverage map includes every google rule", async () => {
  const coverage = await readFile(new URL("../rules/coverage.md", import.meta.url), "utf8");
  for (const question of googleRules.questions) {
    assert.ok(coverage.includes(`\`${question.name}\``), `Missing coverage for ${question.name}`);
  }
});

test("buildCliArgs passes --context only when a case supplies context", () => {
  const withoutContext = buildCliArgs("/tmp/case", { pack: "concurrency", dryRun: false }, { source: "fn a() {}" });
  assert.deepEqual(withoutContext, ["style", "check", "/tmp/case", "--pack", "concurrency", "--format", "json"]);

  const withContext = buildCliArgs(
    "/tmp/case",
    { pack: "concurrency", dryRun: false, ruleName: "stale_read_across_await" },
    { source: "fn a() {}", context: "single consumer" },
  );
  assert.deepEqual(withContext, [
    "style",
    "check",
    "/tmp/case",
    "--pack",
    "concurrency",
    "--format",
    "json",
    "--rule",
    "stale_read_across_await",
    "--context",
    "single consumer",
  ]);
});

test("validateCases rejects concurrency or multi-rule context without context_reason", () => {
  const badCase = [
    {
      name: "missing_reason_bug",
      rule: "stale_read_across_await",
      labels: { stale_read_across_await: "violation" },
      context: "extra info",
      source: "fn bug() {}",
    },
    {
      name: "missing_reason_safe",
      rule: "stale_read_across_await",
      labels: { stale_read_across_await: "no_violation" },
      source: "fn safe() {}",
    },
  ];
  assert.throws(() => validateCases(badCase), /context_reason/);
  assert.throws(() => validateCasePath("../escape.rs", "bad"), /Invalid case path/);
  assert.throws(() => validateCasePath("/abs.rs", "bad"), /Invalid case path/);
  assert.throws(() => validateCasePath("notes.txt", "bad"), /Invalid case path/);
  assert.equal(validateCasePath("src/lib.rs", "ok"), "src/lib.rs");
});

test("parseCliReport extracts findings, line ranges, unresolved reasons, and clean verdicts", () => {
  const parsed = parseCliReport({
    schema_version: 4,
    status: "incomplete",
    provider: "decisions",
    pack: "concurrency",
    model: "gpt-6-luna",
    rules_sha256: "abc",
    context_sha256: null,
    root: "/tmp/example",
    selected_rules: [
      "split_actor_invariant",
      "stale_read_across_await",
      "unbounded_collection_growth",
    ],
    files: [
      {
        path: "lib.rs",
        status: "incomplete",
        cached: false,
        source_sha256: "def",
        findings: [
          {
            rule: "stale_read_across_await",
            lines: { start_line: 12, end_line: 24 },
            guidance: "Stale read",
            confidence: 0.85,
            probabilities: { violation: 0.9, no_violation: 0.08, insufficient_context: 0.02 },
          },
        ],
        unresolved: [
          {
            rule: "split_actor_invariant",
            reason: "insufficient_context",
            confidence: 0.7,
            probabilities: { violation: 0.05, no_violation: 0.25, insufficient_context: 0.7 },
          },
        ],
        no_violation_count: 1,
        input_tokens: 950,
        error: null,
      },
    ],
  });

  assert.equal(parsed.inputTokens, 950);
  assert.equal(parsed.verdicts.get("stale_read_across_await").predicted, "violation");
  assert.deepEqual(parsed.verdicts.get("stale_read_across_await").lines, { start_line: 12, end_line: 24 });
  assert.equal(parsed.verdicts.get("stale_read_across_await").pViolation, 0.9);
  assert.equal(parsed.verdicts.get("split_actor_invariant").predicted, "insufficient_context");
  assert.equal(parsed.verdicts.get("unbounded_collection_growth").predicted, "no_violation");
  assert.equal(parsed.verdicts.get("unbounded_collection_growth").pViolation, null);

  assert.throws(
    () =>
      parseCliReport({
        schema_version: 4,
        status: "incomplete",
        pack: "concurrency",
        selected_rules: ["split_actor_invariant"],
        files: [{ path: "lib.rs", status: "error", error: "provider failed" }],
      }),
    /provider failed/,
  );
});

test("evaluateCaseWithCli writes the source to a temp file and invokes the CLI binary", async () => {
  const tempDir = await mkdtemp(path.join(os.tmpdir(), "adamantium-stub-"));
  const stubCli = path.join(tempDir, "fake-cli.mjs");
  await writeFile(
    stubCli,
    `#!${process.execPath}
import fs from "node:fs";
import path from "node:path";
const args = process.argv.slice(2);
const target = args[2];
const source = fs.readFileSync(path.join(target, "lib.rs"), "utf8");
const ctxIdx = args.indexOf("--context");
const hasContext = ctxIdx !== -1;
const findings = source.includes("BUG") ? [{
  rule: "stale_read_across_await",
  lines: { start_line: 1, end_line: 3 },
  guidance: "stale",
  confidence: 0.9,
  probabilities: { violation: 0.95, no_violation: 0.04, insufficient_context: 0.01 }
}] : [];
const report = {
  schema_version: 4,
  status: "complete",
  provider: "decisions",
  pack: "concurrency",
  model: "gpt-6-luna",
  rules_sha256: "sha",
  context_sha256: hasContext ? "ctx" : null,
  root: target,
  selected_rules: ["split_actor_invariant", "stale_read_across_await", "unbounded_collection_growth"],
  files: [{
    path: "lib.rs",
    status: "checked",
    cached: false,
    source_sha256: "src",
    findings,
    unresolved: [],
    no_violation_count: 3 - findings.length,
    input_tokens: 420,
    error: null
  }]
};
process.stdout.write(JSON.stringify(report));
process.exit(findings.length > 0 ? 1 : 0);
`,
    "utf8",
  );
  await chmod(stubCli, 0o755);

  try {
    const result = await evaluateCaseWithCli(
      { name: "secret_case_name", source: "fn f() { /* BUG */ }\n" },
      { pack: "concurrency", cliPath: stubCli, dryRun: false },
    );
    assert.equal(result.verdicts.get("stale_read_across_await").predicted, "violation");
    assert.deepEqual(result.verdicts.get("stale_read_across_await").lines, { start_line: 1, end_line: 3 });
    assert.equal(result.verdicts.get("split_actor_invariant").predicted, "no_violation");
    assert.equal(result.inputTokens, 420);
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
});

test("a dry run can select examples for one google rule without contacting the API", () => {
  const script = fileURLToPath(new URL("./evaluate-decisions.mjs", import.meta.url));
  const output = execFileSync(process.execPath, [script, "--dry-run", "--rule", "glob_import_context"], {
    encoding: "utf8",
  });
  assert.match(output, /3 selected \(3 rule labels, 3 with --context\) without making API requests/);
});

test("concurrency corpus has valid dev and whole-file held_out splits", () => {
  const labeledRules = validateCases(concurrencyCases, { requireContextReason: true });
  for (const question of concurrencyRules.questions) {
    assert.ok(labeledRules.has(question.name), `Missing examples for ${question.name}`);
  }

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
    assert.equal(example.source.includes(example.name), false, `Case name leaked in source of ${example.name}`);
    if (example.context) {
      assert.equal(example.context.includes(example.name), false, `Case name leaked in context of ${example.name}`);
    }

    if (example.pair !== null) {
      const key = `${example.rule}::${example.split}::${example.pair}`;
      if (!pairs.has(key)) pairs.set(key, []);
      pairs.get(key).push(example);
    }
  }

  for (const [key, group] of pairs.entries()) {
    assert.equal(group.length, 2, `Pair ${key} must have exactly two cases`);
    const outcomes = new Set(group.map((item) => getCaseLabels(item).get(item.rule)));
    assert.deepEqual(outcomes, new Set(["violation", "no_violation"]), `Pair ${key} must pair violation and no_violation`);
  }

  const heldOut = concurrencyCases.filter((item) => item.split === "held_out");
  assert.ok(heldOut.length >= 40, `Expected at least 40 whole-file held_out cases, got ${heldOut.length}`);
  const over150Lines = heldOut.filter((item) => item.source.split("\n").length >= 150);
  assert.ok(over150Lines.length >= 4, `Expected multiple held_out files >= 150 lines, got ${over150Lines.length}`);

  for (const example of heldOut) {
    assert.ok(example.labels, `Held-out case ${example.name} must label every rule in the pack`);
    for (const ruleName of packCandidateRules) {
      assert.ok(
        Object.hasOwn(example.labels, ruleName),
        `Held-out case ${example.name} is missing label for ${ruleName}`,
      );
    }
  }

  for (const ruleName of packCandidateRules) {
    const ruleHeldOut = heldOut.filter((item) => item.rule === ruleName);
    const slices = new Set(ruleHeldOut.map((item) => item.slice));
    assert.deepEqual(slices, validSlices, `Rule ${ruleName} held_out must cover all four slices`);
  }
});

test("the concurrency evaluation report covers every candidate rule", async () => {
  const report = await readFile(new URL("../rules/concurrency-evaluation.md", import.meta.url), "utf8");
  for (const question of concurrencyRules.questions) {
    assert.ok(report.includes(`\`${question.name}\``), `Missing report coverage for ${question.name}`);
  }
});

test("a dry run can select the concurrency held_out split without contacting the API", () => {
  const script = fileURLToPath(new URL("./evaluate-decisions.mjs", import.meta.url));
  const output = execFileSync(
    process.execPath,
    [script, "--dry-run", "--pack", "concurrency", "--split", "held_out"],
    { encoding: "utf8" },
  );
  assert.match(output, /66 selected \(264 rule labels, 1 with --context\) without making API requests/);
});

test("summarizeResults computes precision, recall, unresolved, line ranges, pairs, and slices", () => {
  const summary = summarizeResults([
    {
      name: "p1_bug",
      rule: "r1",
      expected: "violation",
      predicted: "violation",
      slice: "semantic_only",
      pair: "p1",
      existing_lint: null,
      pViolation: 0.9,
      confidence: 0.9,
      lines: { start_line: 10, end_line: 20 },
    },
    {
      name: "p1_safe",
      rule: "r1",
      expected: "no_violation",
      predicted: "no_violation",
      slice: "semantic_only",
      pair: "p1",
      existing_lint: null,
      pViolation: null,
      confidence: null,
      lines: null,
    },
    {
      name: "p2_bug",
      rule: "r1",
      expected: "violation",
      predicted: "no_violation",
      slice: "semantic_only",
      pair: "p2",
      existing_lint: null,
      pViolation: null,
      confidence: null,
      lines: null,
    },
    {
      name: "p2_safe",
      rule: "r1",
      expected: "no_violation",
      predicted: "violation",
      slice: "semantic_only",
      pair: "p2",
      existing_lint: null,
      pViolation: 0.7,
      confidence: 0.7,
      lines: { start_line: 5, end_line: 8 },
    },
    {
      name: "lint_hit",
      rule: "r1",
      expected: "violation",
      predicted: "violation",
      slice: "lint_overlap",
      pair: null,
      existing_lint: "clippy::disallowed_types",
      pViolation: 0.95,
      confidence: 0.95,
      lines: null,
    },
    {
      name: "decoy",
      rule: "r1",
      expected: "no_violation",
      predicted: "no_violation",
      slice: "lint_decoy",
      pair: null,
      existing_lint: "clippy::disallowed_methods",
      pViolation: null,
      confidence: null,
      lines: null,
    },
    {
      name: "insuff",
      rule: "r1",
      expected: "insufficient_context",
      predicted: "violation",
      slice: "insufficient_context",
      pair: null,
      existing_lint: null,
      pViolation: 0.6,
      confidence: 0.6,
      lines: null,
    },
    {
      name: "refused",
      rule: "r1",
      expected: "violation",
      predicted: "refusal",
      slice: "semantic_only",
      pair: null,
      existing_lint: null,
      pViolation: null,
      confidence: null,
      lines: null,
    },
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
  assert.equal(r1.unresolved, 1);
  assert.equal(r1.findings, 4);
  assert.equal(r1.locatedFindings, 2);
  assert.equal(r1.locatedTrueFindings, 1);
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

  const zeroTp = summarizeResults([
    {
      name: "miss",
      rule: "r2",
      expected: "violation",
      predicted: "no_violation",
      slice: "semantic_only",
      pair: "p1",
      existing_lint: null,
      pViolation: null,
      confidence: null,
      lines: null,
    },
    {
      name: "false_alarm",
      rule: "r2",
      expected: "no_violation",
      predicted: "violation",
      slice: "semantic_only",
      pair: "p1",
      existing_lint: null,
      pViolation: 0.8,
      confidence: 0.8,
      lines: null,
    },
  ]);
  assert.equal(zeroTp.perRule.r2.precision, 0);
  assert.equal(zeroTp.perRule.r2.recall, 0);
  assert.equal(zeroTp.perRule.r2.f1, 0);

  const emptyPositives = summarizeResults([
    {
      name: "clean",
      rule: "r3",
      expected: "no_violation",
      predicted: "no_violation",
      slice: "semantic_only",
      pair: null,
      existing_lint: null,
      pViolation: null,
      confidence: null,
      lines: null,
    },
  ]);
  assert.equal(emptyPositives.perRule.r3.precision, null);
  assert.equal(emptyPositives.perRule.r3.recall, null);
  assert.equal(emptyPositives.perRule.r3.f1, null);
});
