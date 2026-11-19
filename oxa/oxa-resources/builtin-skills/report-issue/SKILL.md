---
name: report-issue
description: Draft a bug report for the public ox-llm repository from scrubbed system and session facts (no email, no home paths, no prompts or transcripts, no secrets). Shows the user the exact title and body and posts only after the user confirms in their own terminal. Use when the user wants to report a problem with ox-llm, oxa or the engine.
---

# Report an ox-llm issue

Never post anything yourself and never paste conversation text, prompts, file contents, paths
under the home directory, names, emails, keys or tokens into a report.

1. Ask the user for a one-line title and, in their words, what they did, what they expected and
   what happened. Keep it short; no transcripts and no prompt text.
2. Write that description to a file in the session's temporary directory and draft the issue:

   ```
   oxa report-issue draft --title "TITLE" --notes-file NOTES.txt
   ```

   The command collects scrubbed facts (oxa version, machine, OS, and the drafting session's
   model with its limits and engine state, read in-process: no controller CLI is run), scrubs
   the notes too, saves the draft and prints the draft path followed by the exact title and
   body.

   Title the report with the symptom and the component, without prefixes (no "Bug:", no
   "[oxa]") that make every report look alike.
3. For dogfood reports, use semantic dedupe. The owner enables
   `oxa.reportIssue.semanticDedupe` in user settings. Include these fields in the draft notes,
   using stable task and worker identifiers and a distinct identifier for each run.

   ```text
   Task: example-task
   Worker: example-worker
   Run: example-run
   Observed: The command and one line describing the actual failure.
   Evidence: path/to/source.rs:42
   ```

   Submit checks the cited source against current main and asks the configured model to compare
   candidates from open issues and issues closed in the past 30 days. It previews the selected
   comment or new issue before asking for permission to post. It creates at most one new issue
   per task and three per worker per UTC day in the local agent directory. Drafts that fail
   verification or hit a cap stay local, with the reason shown at submit time.

   For other reports, look for a duplicate before the user posts anything. Take two or three keyword sets from
   the symptom (error text, command, component), not only the planned title, and search the
   OPEN issues and OPEN pull requests of the target repository (`oxa.reportIssue.repo`), then
   read the top matches:

   ```
   gh issue list -R OWNER/NAME --state open --search "KEYWORDS" --json number,title
   gh pr list -R OWNER/NAME --state open --search "KEYWORDS" --json number,title
   ```

   - It matches an existing issue: do not file. Tell the user to add a comment with
     `gh issue comment NUMBER -R OWNER/NAME` containing only what is new (environment,
     another reproduction, frequency), and say which issue it is. If an open pull request
     already fixes it, say so and give its link.
   - It is related but different: file, and link the related issues in the body.
   - The search fails (no network, no `gh` access): say so and keep the draft as a local file;
     do not file blind.
4. Show the user the printed title and body exactly as printed, and the draft path. Ask whether
   anything should be removed. To change it, edit the notes and run the draft command again.
5. Posting needs the user's explicit confirmation. Tell the user to run, in their own terminal:

   ```
   oxa report-issue submit DRAFT-PATH
   ```

   It prints the text again and posts the issue or comment only after the user types `yes` on
   the terminal. It refuses when there is no terminal, so running it from a tool cannot post.
6. Reporting is off until the public repository exists: `submit` then says so and leaves the
   draft in place so the user can file it by hand. The target is the setting
   `oxa.reportIssue.repo` (`OWNER/NAME`) in `~/.oxa/agent/settings.json`.
