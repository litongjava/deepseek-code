Clean up the changed code without changing its behavior: $ARGUMENTS

Start from the actual changes: run `git status` and `git diff` (and `git diff --staged`). With no changes, ask what to simplify instead of guessing.

Review only the changed code for:
- reuse: helpers that already exist in the project and duplicated logic that should call them;
- simplification: needless branches, nesting, temporary variables, conversions, dead code and comments that restate the code;
- efficiency: repeated work in loops, redundant reads or allocations, and avoidable blocking calls.

Apply the improvements directly, matching the surrounding style. Keep public behavior, error handling and tests' expectations unchanged; do not reformat unrelated code. Run the project's existing tests or type checks that cover the changed files, and report what changed and what was verified. If an improvement would change behavior, list it as a suggestion instead of making it.
