---
name: kk-changelog
description: Draft changelog entries and release notes from the git history since the last tag.
version: 1.0.0
triggers: [changelog, release notes]
---

# Changelog

Turn the commits since the last release into user-facing notes.

## Steps

1. Read `references/format.md` for the section layout this project uses.
2. Collect the history: `git log <last-tag>..HEAD --oneline --no-merges`.
3. Group commits by what the user can now do, not by which file changed. Drop
   refactor and chore commits unless they changed observable behaviour.
4. Write one imperative line per change, oldest first, and cite the commit
   short sha at the end of the line.
5. Output Markdown ready to paste at the top of the changelog file. Do not edit
   the file unless the user asked you to.
