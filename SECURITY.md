# Security Policy

## Reporting a Vulnerability

Please **do not** report security vulnerabilities through public GitHub issues or pull requests.

Instead, use **GitHub Private Vulnerability Reporting** on this repository (`Security` -> `Advisories` -> `Report a vulnerability`).

Please include:
- A clear description of the vulnerability and affected component/file.
- Reproduction steps or a minimal proof of concept.
- Potential impact (e.g., credential exposure, sandbox escape, command injection).

## Scope Notes

- `hrn` intentionally blocks consumer Google Gemini CLI / Antigravity OAuth client flows and third-party CLI credential harvesting; bypassing those guardrails is treated as a policy violation.
- Never submit real API keys, tokens, cookies, or private credentials in issues, PRs, or advisories.
