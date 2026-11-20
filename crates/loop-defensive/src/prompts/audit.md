Audit the GitHub repository $1 for security vulnerabilities and give me a report. Use only the `mcp__defensive-audit__*` tools. Do not use Loop's own read, write, edit, bash, grep, find or ls tools: `mcp__defensive-audit__read` reads the read-only, network-less sandbox. You are reporting vulnerabilities, not fixing them.

1. Call `mcp__defensive-audit__prepare_audit` with url "$1". Show me the stage reports it prints. If it says the repo cannot be audited, explain why and stop.
2. Map the code: `mcp__defensive-audit__list_directory` and `mcp__defensive-audit__search_code` to find untrusted-input entry points (HTTP handlers, parsers, deserialization, exec, SQL, file paths, crypto) and where that data flows.
3. Call `mcp__defensive-audit__run_analyzer` with "all" to get static-analyzer leads (Go: gosec, go vet, govulncheck; Python: Bandit; JavaScript: npm audit). Confirm or dismiss each by reading the code with `mcp__defensive-audit__read`; do not just repeat them.
4. For each real issue, call `mcp__defensive-audit__submit_finding` with a title, severity, CWE, the file and line range you have read and verified, a description, the data flow (source to sink), the reachability (can untrusted input actually reach it, and under what conditions: this is the exploitability assessment), the impact, and a recommendation.
5. Call `mcp__defensive-audit__submit_audit_report` with the summary, scope, methodology and overall risk.
6. Call `mcp__defensive-audit__finish_audit`. Show me its report.
7. Summarize for me: the highest-severity issues, their reachability, and the overall risk. Remind me these are candidates for human review, not a verdict.
