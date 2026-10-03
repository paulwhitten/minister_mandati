# Security Policy

`mima` runs model-requested shell commands and file operations on your machine,
so security reports are welcome.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
("Report a vulnerability" under the repository's Security tab). Do not open a
public issue for an unpatched vulnerability.

## Security model

- Shell commands and file writes require interactive approval by default
  (`[security]` in `agent.toml`). Settings that relax this
  (`require_approval_for_*`, `auto_approve_bash`) are the operator's choice.
- `allowed_paths` confines the filesystem tools only. It is **not** a shell
  sandbox: an approved shell command runs with your user's full permissions.
- The agent sends data only to the configured `base_url`. There is no
  telemetry.
- Transcripts (off by default) are written locally with mode 0600 under a
  0700 directory and can contain source code and command output. They are
  never transmitted.

In scope: sandbox escapes in the filesystem tools, approval bypasses (including
`auto_approve_bash` matching), and unintended network traffic. Out of scope:
damage from shell commands the operator explicitly approved.
