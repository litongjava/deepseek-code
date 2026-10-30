# Repository Guidelines

## Project Structure & Architecture

- `src/`: React/TypeScript UI, API adapters, hooks, stores, and colocated tests. Assets live in `src/assets/` and `public/`.
- `crates/core/`: shared Rust engine, persistence, permissions, and services. Keep business logic here, independent of Tauri.
- `crates/cli/`: standalone `dcl` executable; it must work without Desktop or the port 30085 Bridge.
- `src-tauri/`: Desktop adapters and Bridge routes; `src-tauri/worker/` contains the computer-operation helper.
- `scripts/`: build orchestration; `docs/`: architecture and migration references. `legacy/claude-code-cli/` is a read-only historical snapshot, excluded from the workspace.

## Build, Test, and Development Commands

Use Node.js 22+, Rust, and the platform's Tauri prerequisites.

- `npm ci`: install locked frontend dependencies.
- `npm run tauri -- dev`: launch Desktop with the Vite development server.
- `npm run dev`: run the frontend development server alone.
- `npm run build:all`: build CLI, Desktop, and installers into `artifacts/`.
- `cargo build --locked -p deepseek-code-cli`: build only the standalone CLI.
- `npx tsc --noEmit`: check TypeScript types.
- `npm run test:run`: execute frontend tests once.
- `cargo test --locked -p deepseek-code-core --lib --tests`: run core tests; use `-p deepseek-code-cli` or `-p deepseek-code-desktop` for those packages.
- `cargo check --locked -p deepseek-code-desktop --all-targets`: check Desktop targets.

Cargo output is configured under the workspace root `target/`.

## Coding Style & Naming

Match surrounding code: TypeScript uses two-space indentation, PascalCase components, and camelCase functions/hooks. Rust uses four-space indentation, snake_case modules/functions, and PascalCase types. Format changed Rust code with `rustfmt`; avoid unrelated formatting churn. No ESLint or Prettier configuration is currently provided. Run `git diff --check` before committing.

## Testing Guidelines

Frontend tests use Vitest, jsdom, and React Testing Library; name files `*.test.ts(x)` or `*.spec.ts(x)`. Rust integration tests live in each crate's `tests/` directory. No numeric coverage threshold is configured. Test changed behavior, failures, cancellation, and persistence where applicable; use temporary directories and local mock endpoints instead of real credentials.

## Commits & Pull Requests

Use English imperative messages with existing prefixes such as `feat:` and `fix:`. Describe the problem, resulting behavior, and validation in PRs; link relevant issues and include screenshots for UI changes. Identify unverified platforms or external integrations explicitly.

## Agent & Documentation Instructions

Read PowerShell text explicitly as UTF-8; avoid console tables. `D:/code/project/project-harven/claude-code-cli-doc` holds end-user CLI documentation and `D:/code/project/project-harven/claude-rust-desktop-docs` holds Desktop user documentation; write development records, acceptance evidence and test scripts under `D:/code/project/project-harven/temp/claude-code-cli-dev`. For browser testing, use `D:/code/project/project-browser-use/deepseek-browser-use`, read its skills, and show the test process. Store test screenshots in that development directory's `assets/` folder. Continue CLI work from `docs/cli-remaining-work.md`. Never commit credentials or modify the original CLI repository.
