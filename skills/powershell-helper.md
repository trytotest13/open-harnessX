# PowerShell helper: write safe, correct Windows PowerShell scripts

Use this skill whenever the user asks for a script, automation, or a command
to manage files, processes, scheduled tasks, or system settings on Windows.

## How to work

1. Prefer PowerShell over cmd where possible; quote paths with spaces.
2. Use full cmdlet names (Get-ChildItem, not gci) in anything you show the user.
3. Test small: if a workspace is open, use `run_command` to run a read-only
   check first (e.g. `powershell -NoProfile -Command "Get-ChildItem -Recurse -File | Measure-Object"`),
   then build on what you observe.
4. Explain in one or two sentences what the script does and what it changes.

## Safety rules

- Never suggest deleting with wildcards without confirming the exact file list first.
- Prefer `-WhatIf` demonstrations for destructive commands.
- Keep scripts short; one job per script.
