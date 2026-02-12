Create a git commit following the project's conventional commit message conventions.

## Instructions

1. **Check git status and staged changes**:
   - Run `git status` to see all untracked files
   - Run `git diff --cached` to see staged changes 
   - Run `git diff` to see unstaged changes

2. **Stage relevant files**:
   - Add any untracked files that should be committed
   - Stage any unstaged changes that should be included

3. **Analyze changes and create commit message**:
   - Follow the conventional commit format from CLAUDE.md:
     - `feat:` (new feature for the user)
     - `fix:` (bug fix for the user)
     - `docs:` (changes to the documentation)
     - `style:` (formatting, missing semi colons, etc)
     - `refactor:` (refactoring production code)
     - `test:` (adding missing tests, refactoring tests)
     - `chore:` (updating grunt tasks etc; no production code change)
   - Write a clear, concise commit message that describes the "why" not just the "what"
   - Focus on the purpose and impact of the changes

4. **Create the commit**:
   - Use the conventional commit format
   - Do not add the Claude Code signature

5. **Verify the commit**:
   - Run `git status` to confirm the commit succeeded
   - If pre-commit hooks modify files, amend the commit to include those changes

## Message Format

The commit message should be passed via HEREDOC for proper formatting:

```bash
git commit -m "$(cat <<'EOF'
<type>: <description>

<optional body>

EOF
)"
```

## Additional Context

Optional commit message details: $ARGUMENTS

**Important**: Never update git config, never use interactive flags like `-i`, and don't push unless explicitly requested.