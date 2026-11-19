import { spawn } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const labels = ["violation", "no_violation", "insufficient_context"];
const validPacks = new Set(["google", "concurrency"]);

/** Validate a relative `.rs` path so a case cannot write outside its temp dir. */
export function validateCasePath(relPath, caseName) {
  if (typeof relPath !== "string" || !relPath || path.isAbsolute(relPath) || relPath.includes("\\")) {
    throw new Error(`Invalid case path on ${caseName}: ${relPath}`);
  }
  const normalized = path.posix.normalize(relPath);
  if (normalized === "." || normalized.startsWith("../") || normalized === ".." || !normalized.endsWith(".rs")) {
    throw new Error(`Invalid case path on ${caseName}: ${relPath}`);
  }
  return normalized;
}

/** Return a Map of rule -> expected label for either a single-rule or multi-rule case. */
export function getCaseLabels(example) {
  const result = new Map();
  if (example.labels !== undefined) {
    for (const [rule, expected] of Object.entries(example.labels)) {
      result.set(rule, expected);
    }
    return result;
  }
  result.set(example.rule, example.expected);
  return result;
}

/** Validate labeled cases before invoking the CLI. */
export function validateCases(cases, options = {}) {
  const { requireContextReason = false, requiredRules = null } = options;
  if (!Array.isArray(cases) || cases.length === 0) {
    throw new Error("No examples were supplied.");
  }

  const caseNames = new Set();
  const ruleOutcomes = new Map();

  for (const example of cases) {
    if (!example || typeof example !== "object") {
      throw new Error("Every example must be an object.");
    }
    if (typeof example.name !== "string" || !example.name.trim() || caseNames.has(example.name)) {
      throw new Error(`Invalid or duplicate example name: ${example.name ?? "unnamed"}`);
    }
    caseNames.add(example.name);

    if (typeof example.source !== "string" || !example.source.trim()) {
      throw new Error(`Missing source on ${example.name}`);
    }
    if (example.path !== undefined && example.path !== null) {
      validateCasePath(example.path, example.name);
    }

    const hasContext = example.context !== undefined && example.context !== null;
    const hasContextReason = example.context_reason !== undefined && example.context_reason !== null;
    if (hasContext && (typeof example.context !== "string" || !example.context.trim())) {
      throw new Error(`Invalid context on ${example.name}`);
    }
    if (hasContextReason && (typeof example.context_reason !== "string" || !example.context_reason.trim())) {
      throw new Error(`Invalid context_reason on ${example.name}`);
    }
    if (hasContextReason && !hasContext) {
      throw new Error(`context_reason set without context on ${example.name}`);
    }

    const isMultiRule = example.labels !== undefined;
    if (hasContext && (requireContextReason || isMultiRule) && !hasContextReason) {
      throw new Error(
        `Case ${example.name} supplies context without context_reason; pass --context only when the label depends on it and say why.`,
      );
    }

    if (isMultiRule) {
      if (!example.labels || typeof example.labels !== "object" || Array.isArray(example.labels)) {
        throw new Error(`Invalid labels object on ${example.name}`);
      }
      const entries = Object.entries(example.labels);
      if (entries.length === 0) {
        throw new Error(`Empty labels object on ${example.name}`);
      }
      for (const [rule, expected] of entries) {
        if (!rule || !labels.includes(expected)) {
          throw new Error(`Invalid label for rule ${rule} on ${example.name}`);
        }
        if (!ruleOutcomes.has(rule)) ruleOutcomes.set(rule, new Set());
        ruleOutcomes.get(rule).add(expected);
      }
      if (example.rule !== undefined && example.rule !== null) {
        if (typeof example.rule !== "string" || !Object.hasOwn(example.labels, example.rule)) {
          throw new Error(`Primary rule ${example.rule} is missing from labels on ${example.name}`);
        }
        if (example.expected !== undefined && example.expected !== example.labels[example.rule]) {
          throw new Error(`Primary expected label disagrees with labels[${example.rule}] on ${example.name}`);
        }
      }
      if (requiredRules) {
        for (const required of requiredRules) {
          if (!Object.hasOwn(example.labels, required)) {
            throw new Error(`Case ${example.name} is missing required rule label ${required}`);
          }
        }
      }
    } else {
      if (typeof example.rule !== "string" || !example.rule || !labels.includes(example.expected)) {
        throw new Error(`Invalid single-rule example: ${example.name}`);
      }
      if (requiredRules) {
        throw new Error(`Case ${example.name} must provide a labels map for all required rules.`);
      }
      if (!ruleOutcomes.has(example.rule)) ruleOutcomes.set(example.rule, new Set());
      ruleOutcomes.get(example.rule).add(example.expected);
    }
  }

  for (const [rule, outcomes] of ruleOutcomes.entries()) {
    if (!outcomes.has("violation") || !outcomes.has("no_violation")) {
      throw new Error(`Rule ${rule} needs both a positive and a negative example.`);
    }
  }

  return new Set(ruleOutcomes.keys());
}

/** Build the exact `cargo adamantium style check` arguments for one case. */
export function buildCliArgs(targetPath, config, example) {
  const args = ["style", "check", targetPath, "--pack", config.pack, "--format", "json"];
  if (config.ruleName) {
    args.push("--rule", config.ruleName);
  }
  if (typeof example.context === "string" && example.context.trim().length > 0) {
    args.push("--context", example.context);
  }
  if (config.dryRun) {
    args.push("--dry-run");
  }
  return args;
}

/** Validate and parse a `cargo adamantium style check --format json` report for a single file. */
export function parseCliReport(report) {
  if (!report || typeof report !== "object") {
    throw new Error("CLI report must be a JSON object.");
  }
  if (report.schema_version !== 4) {
    throw new Error(`Unsupported CLI report schema version: ${report.schema_version}`);
  }
  if (!Array.isArray(report.selected_rules) || report.selected_rules.length === 0) {
    throw new Error("CLI report is missing selected_rules.");
  }
  if (!Array.isArray(report.files) || report.files.length !== 1) {
    throw new Error(`Expected exactly 1 file in CLI report, got ${report.files?.length ?? 0}.`);
  }

  const file = report.files[0];
  if (file.status === "error" || file.status === "not_checked") {
    throw new Error(`CLI review failed for ${file.path}: ${file.error ?? file.status}`);
  }

  const selected = new Set(report.selected_rules);
  const verdicts = new Map();

  if (file.status !== "planned") {
    for (const finding of file.findings ?? []) {
      if (!selected.has(finding.rule) || verdicts.has(finding.rule)) {
        throw new Error(`Unexpected or duplicate finding rule in CLI report: ${finding.rule}`);
      }
      const lines = finding.lines
        ? { start_line: finding.lines.start_line, end_line: finding.lines.end_line }
        : null;
      if (
        lines &&
        (!Number.isInteger(lines.start_line) ||
          !Number.isInteger(lines.end_line) ||
          lines.start_line < 1 ||
          lines.end_line < lines.start_line)
      ) {
        throw new Error(`Invalid line range for ${finding.rule}: ${JSON.stringify(lines)}`);
      }
      const pViolation = finding.probabilities?.violation;
      verdicts.set(finding.rule, {
        predicted: "violation",
        confidence: typeof finding.confidence === "number" ? finding.confidence : null,
        pViolation: typeof pViolation === "number" ? pViolation : null,
        probabilities: finding.probabilities ?? null,
        lines,
      });
    }

    for (const item of file.unresolved ?? []) {
      if (!selected.has(item.rule) || verdicts.has(item.rule)) {
        throw new Error(`Unexpected or duplicate unresolved rule in CLI report: ${item.rule}`);
      }
      let predicted;
      if (item.reason === "insufficient_context") {
        predicted = "insufficient_context";
      } else if (item.reason === "refusal") {
        predicted = "refusal";
      } else {
        throw new Error(`Unknown unresolved reason for ${item.rule}: ${item.reason}`);
      }
      const pViolation = item.probabilities?.violation;
      verdicts.set(item.rule, {
        predicted,
        confidence: typeof item.confidence === "number" ? item.confidence : null,
        pViolation: typeof pViolation === "number" ? pViolation : null,
        probabilities: item.probabilities ?? null,
        lines: null,
      });
    }

    let cleanCount = 0;
    for (const rule of report.selected_rules) {
      if (!verdicts.has(rule)) {
        cleanCount += 1;
        verdicts.set(rule, {
          predicted: "no_violation",
          confidence: null,
          pViolation: null,
          probabilities: null,
          lines: null,
        });
      }
    }

    if (cleanCount !== file.no_violation_count) {
      throw new Error(
        `CLI report no_violation_count (${file.no_violation_count}) disagrees with clean rules (${cleanCount}).`,
      );
    }
  }

  return {
    pack: report.pack,
    model: report.model,
    rulesSha256: report.rules_sha256,
    selectedRules: report.selected_rules,
    fileStatus: file.status,
    inputTokens: typeof file.input_tokens === "number" ? file.input_tokens : 0,
    verdicts,
  };
}

/** Invoke the built CLI binary (or `cargo adamantium`) and return stdout, stderr, and exit code. */
export function runCliCommand(cliPath, args) {
  const command = cliPath || "cargo";
  const fullArgs = cliPath ? args : ["adamantium", ...args];
  return new Promise((resolve, reject) => {
    const child = spawn(command, fullArgs, {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", (error) => reject(error));
    child.on("close", (code, signal) => {
      if (signal) {
        reject(new Error(`CLI terminated by signal ${signal}: ${stderr.trim()}`));
        return;
      }
      resolve({ code: code ?? 2, stdout, stderr });
    });
  });
}

/** Write one case to a fresh temp directory, run the CLI on it, and parse the report. */
export async function evaluateCaseWithCli(example, config, runner = runCliCommand) {
  const relPath = example.path ? validateCasePath(example.path, example.name) : "lib.rs";
  const tempDir = await mkdtemp(path.join(os.tmpdir(), "adamantium-eval-"));
  try {
    const filePath = path.join(tempDir, ...relPath.split("/"));
    await mkdir(path.dirname(filePath), { recursive: true });
    await writeFile(filePath, example.source, "utf8");

    const args = buildCliArgs(tempDir, config, example);
    const { code, stdout, stderr } = await runner(config.cliPath, args);
    if (code !== 0 && code !== 1 && code !== 2) {
      throw new Error(`CLI exited with unexpected status ${code}: ${stderr.trim()}`);
    }

    let rawReport;
    try {
      rawReport = JSON.parse(stdout);
    } catch {
      throw new Error(
        `CLI exited ${code} without a JSON report for ${example.name}: ${stderr.trim() || stdout.trim()}`,
      );
    }

    const parsed = parseCliReport(rawReport);
    if (parsed.pack !== config.pack) {
      throw new Error(`CLI report pack (${parsed.pack}) does not match requested pack (${config.pack}).`);
    }
    return parsed;
  } finally {
    await rm(tempDir, { recursive: true, force: true });
  }
}

/** Compute precision, recall, unresolved counts, line-range coverage, pair discrimination, and slice metrics. */
export function summarizeResults(records) {
  const computeBucket = (items) => {
    let exactMatches = 0;
    let unresolved = 0;
    let predictedInsufficient = 0;
    let refusals = 0;
    let findings = 0;
    let locatedFindings = 0;
    let locatedTrueFindings = 0;
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
      if (item.predicted === "refusal") {
        refusals += 1;
        unresolved += 1;
      } else if (item.predicted === "insufficient_context") {
        predictedInsufficient += 1;
        unresolved += 1;
      }

      if (!confusion[item.expected]) confusion[item.expected] = {};
      confusion[item.expected][item.predicted] = (confusion[item.expected][item.predicted] ?? 0) + 1;

      const expectedPositive = item.expected === "violation";
      const predictedPositive = item.predicted === "violation";
      const hasLines = Boolean(item.lines && Number.isInteger(item.lines.start_line));
      if (predictedPositive) {
        findings += 1;
        if (hasLines) locatedFindings += 1;
      }

      if (expectedPositive && predictedPositive) {
        tp += 1;
        if (hasLines) locatedTrueFindings += 1;
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
        slices[sliceName] = { total: 0, exactMatches: 0, predictedViolation: 0, unresolved: 0, tp: 0, fp: 0, fn: 0 };
      }
      const sliceStats = slices[sliceName];
      sliceStats.total += 1;
      if (item.predicted === item.expected) sliceStats.exactMatches += 1;
      if (predictedPositive) sliceStats.predictedViolation += 1;
      if (item.predicted === "insufficient_context" || item.predicted === "refusal") sliceStats.unresolved += 1;
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
          lines: item.lines ?? null,
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
    const f1Denominator = 2 * tp + fp + fn;
    const f1 = f1Denominator > 0 ? (2 * tp) / f1Denominator : null;

    return {
      total: items.length,
      exactMatches,
      exactAccuracy: items.length > 0 ? exactMatches / items.length : null,
      unresolved,
      predictedInsufficient,
      refusals,
      findings,
      locatedFindings,
      locatedTrueFindings,
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

function formatMetric(value) {
  return value === null ? "n/a" : `${(value * 100).toFixed(1)}%`;
}

export function parseCliArgs(options) {
  const config = {
    pack: "google",
    cliPath: process.env.CARGO_ADAMANTIUM?.trim() || undefined,
    dryRun: false,
    metrics: false,
    ruleName: undefined,
    casesPath: undefined,
    splitName: undefined,
    outputPath: undefined,
  };
  let packSet = false;
  let cliSet = false;
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
    } else if (option === "--pack" && !packSet) {
      const value = takeValue(index, option);
      if (!validPacks.has(value)) {
        throw new Error(`Invalid pack: ${value} (expected google or concurrency)`);
      }
      config.pack = value;
      packSet = true;
      index += 1;
    } else if (option === "--cli" && !cliSet) {
      config.cliPath = path.resolve(takeValue(index, option));
      cliSet = true;
      index += 1;
    } else if (option === "--rule" && config.ruleName === undefined) {
      config.ruleName = takeValue(index, option);
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
        "Usage: node style/scripts/evaluate-decisions.mjs [--pack google|concurrency] [--cli PATH] " +
          "[--cases PATH] [--split NAME] [--rule NAME] [--dry-run] [--metrics] [--output PATH]",
      );
    }
  }
  return config;
}

async function main() {
  const config = parseCliArgs(process.argv.slice(2));
  const defaultCases =
    config.pack === "concurrency" ? "fixtures/concurrency-cases.json" : "fixtures/google-rust-cases.json";
  const casesFile = config.casesPath ? path.resolve(config.casesPath) : path.join(root, defaultCases);
  const cases = JSON.parse(await readFile(casesFile, "utf8"));
  const labeledRules = validateCases(cases, {
    requireContextReason: config.pack === "concurrency",
  });

  if (config.ruleName && !labeledRules.has(config.ruleName)) {
    throw new Error(`Unknown rule in cases: ${config.ruleName}`);
  }

  let selected = cases;
  if (config.ruleName) {
    selected = selected.filter((example) => getCaseLabels(example).has(config.ruleName));
  }
  if (config.splitName !== undefined) {
    selected = selected.filter((example) => example.split === config.splitName);
    if (selected.length === 0) throw new Error(`No examples matched split: ${config.splitName}`);
  }

  if (config.dryRun) {
    let labelCount = 0;
    let contextCount = 0;
    for (const example of selected) {
      const caseLabels = getCaseLabels(example);
      labelCount += config.ruleName ? 1 : caseLabels.size;
      if (typeof example.context === "string" && example.context.trim()) contextCount += 1;
      buildCliArgs("lib.rs", config, example);
    }
    console.log(
      `Validated ${labeledRules.size} labeled rules across ${cases.length} cases; ` +
        `${selected.length} selected (${labelCount} rule labels, ${contextCount} with --context) without making API requests.`,
    );
    return;
  }

  let matches = 0;
  let totalScored = 0;
  let inputTokens = 0;
  let filesChecked = 0;
  let model = null;
  let rulesSha256 = null;
  let selectedPackRules = null;
  const records = [];
  const unlabeledAlerts = [];

  for (const example of selected) {
    const report = await evaluateCaseWithCli(example, config);
    filesChecked += 1;
    inputTokens += report.inputTokens;
    model = report.model;
    rulesSha256 = report.rulesSha256;
    selectedPackRules = report.selectedRules;

    const caseLabels = getCaseLabels(example);
    const lineParts = [];

    for (const ruleName of report.selectedRules) {
      const verdict = report.verdicts.get(ruleName);
      if (!caseLabels.has(ruleName)) {
        if (verdict.predicted !== "no_violation") {
          unlabeledAlerts.push({
            name: example.name,
            rule: ruleName,
            predicted: verdict.predicted,
            pViolation: verdict.pViolation,
            confidence: verdict.confidence,
            lines: verdict.lines,
          });
          lineParts.push(`${ruleName}=${verdict.predicted}(unlabeled!)`);
        }
        continue;
      }

      const expected = caseLabels.get(ruleName);
      const isPrimary = example.rule === ruleName;
      totalScored += 1;
      if (verdict.predicted === expected) matches += 1;

      records.push({
        name: example.name,
        rule: ruleName,
        primaryRule: example.rule ?? null,
        split: example.split ?? null,
        slice: isPrimary ? (example.slice ?? null) : "cross_rule",
        pair: isPrimary ? (example.pair ?? null) : null,
        existing_lint: isPrimary ? (example.existing_lint ?? null) : null,
        usedContext: Boolean(example.context),
        expected,
        predicted: verdict.predicted,
        pViolation: verdict.pViolation,
        confidence: verdict.confidence,
        probabilities: verdict.probabilities,
        lines: verdict.lines,
        inputTokens: report.inputTokens,
      });

      const lineSuffix = verdict.lines ? ` L${verdict.lines.start_line}-${verdict.lines.end_line}` : "";
      const probSuffix = verdict.pViolation !== null ? ` P=${verdict.pViolation.toFixed(3)}` : "";
      if (verdict.predicted !== expected || verdict.predicted !== "no_violation") {
        lineParts.push(`${ruleName}=${verdict.predicted}(exp ${expected}${probSuffix}${lineSuffix})`);
      }
    }

    const detail = lineParts.length > 0 ? lineParts.join(", ") : "all clean";
    console.log(`${example.name}: ${detail} [${report.inputTokens} tok]`);
  }

  console.log(
    `Matched ${matches}/${totalScored} rule labels across ${filesChecked} files; ${inputTokens} input tokens.`,
  );
  if (unlabeledAlerts.length > 0) {
    console.log(`Warning: ${unlabeledAlerts.length} non-clean verdict(s) on unlabeled rule/case pairs.`);
  }

  const summary = summarizeResults(records);
  if (config.metrics) {
    console.log("\nPer-rule metrics (violation = positive):");
    for (const [rule, stats] of Object.entries(summary.perRule)) {
      console.log(
        `  ${rule}: precision=${formatMetric(stats.precision)} (${stats.tp}/${stats.tp + stats.fp}), ` +
          `recall=${formatMetric(stats.recall)} (${stats.tp}/${stats.tp + stats.fn}), ` +
          `F1=${formatMetric(stats.f1)}, unresolved=${stats.unresolved}, ` +
          `located_tp=${stats.locatedTrueFindings}/${stats.tp}, ` +
          `fp_safe=${stats.fpOnSafe}, exact=${stats.exactMatches}/${stats.total}, ` +
          `pairs=${stats.pairs.bothCorrect}/${stats.pairs.total}`,
      );
    }
    const overall = summary.overall;
    console.log(
      `  OVERALL: precision=${formatMetric(overall.precision)} (${overall.tp}/${overall.tp + overall.fp}), ` +
        `recall=${formatMetric(overall.recall)} (${overall.tp}/${overall.tp + overall.fn}), ` +
        `F1=${formatMetric(overall.f1)}, unresolved=${overall.unresolved}, ` +
        `located_tp=${overall.locatedTrueFindings}/${overall.tp}, ` +
        `fp_safe=${overall.fpOnSafe}, exact=${overall.exactMatches}/${overall.total}, ` +
        `pairs=${overall.pairs.bothCorrect}/${overall.pairs.total}`,
    );
  }

  if (config.outputPath) {
    const payload = {
      timestamp: new Date().toISOString(),
      pack: config.pack,
      model,
      rulesSha256,
      selectedRules: selectedPackRules,
      casesPath: path.relative(process.cwd(), casesFile),
      split: config.splitName ?? null,
      ruleFilter: config.ruleName ?? null,
      filesChecked,
      inputTokens,
      unlabeledAlerts,
      summary,
      results: records,
    };
    await writeFile(path.resolve(config.outputPath), `${JSON.stringify(payload, null, 2)}\n`, "utf8");
  }

  if (!config.metrics && (matches !== totalScored || unlabeledAlerts.length > 0)) {
    process.exitCode = 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main().catch((error) => {
    const code = error.cause?.code;
    console.error(code ? `${error.message} (${code})` : error.message);
    process.exitCode = 1;
  });
}
