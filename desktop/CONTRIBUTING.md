# Contributing to Pantheon Desktop

Thanks for helping. Pantheon Desktop is an open-source desktop app (see
[LICENSE](LICENSE)) built with Tauri 2
and a vanilla TypeScript + CSS frontend — no framework, no JSX.

## Getting started

### Prerequisites

| Tool | Version |
|------|---------|
| [Node.js](https://nodejs.org/) | 22 LTS or later |
| [Rust](https://rustup.rs/) | stable (via `rustup`) |
| [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) | platform-specific |

### Clone and run

```sh
git clone https://github.com/k1ng0mar/pantheon.git
cd pantheon/desktop
npm install
npm run desktop          # opens the dev window (Vite + Tauri)
```

`npm run dev` starts the Vite dev server alone (no Tauri shell), handy for
tweaking styles or previewing the UI in a browser.

### Run from a different build

To point the Tauri dev window at a debug build of the Pantheon binary:

```sh
PANTHEON_BIN=/path/to/debug/pantheon npm run desktop
```

### Release build

`npm run package` builds the production bundle (Tauri `tauri build`). To speed
up development iterations use `npm run package:fast` which uses a
`fast-release` Cargo profile.

## Architecture at a glance

| Layer | Path | Notes |
|-------|------|-------|
| Frontend | `src/` | Vanilla TS + CSS; one HTML entry; imports bundled by Vite |
| Tauri shell | `src-tauri/` | Rust IPC bridge; calls the Pantheon runtime crate |
| Preview mode | `npm run dev` | Frontend only, with mock data from `src/preview.ts` |
| Relay | `relay/` | Cloudflare Worker for Pantheon Connect |

The frontend is intentionally framework-free. Each feature lives in its own
`.ts` + `.css` pair. `main.ts` wires them together.

## Coding style

- **No framework imports.** DOM helpers live in `dom.ts`.
- **Keep files focused.** One feature = one `.ts` + one `.css`.
- TypeScript strict mode, no `any` without a comment.
- CSS custom properties for tokens; no Tailwind.

## Tests

```sh
npm test                     # frontend unit tests (node --test)
npm run test:rust            # Tauri Rust tests
npx playwright test          # full E2E suite; starts Vite itself
npx playwright test --config playwright.ci.config.ts   # the CI subset
```

Playwright runs against `npm run dev` (preview mode, mock data). On a small
machine set `PW_WORKERS=1`; the default of 4 oversubscribes a 4-vCPU host and
the clock-driven specs time out.

CI runs the subset in `playwright.ci.config.ts`, not the full suite. The full
suite has 74 spec files; 32 of them assert a provider list, a settings shape,
or UI text from before the rebrand and fail deterministically against the
current app. They are excluded from CI, not deleted, and
`docs/developer/ci.md` lists each one with its cause. Repair a spec by
updating its expectations to the app as it is now, then remove it from
`testIgnore`.

## Commits and PRs

- Lowercase subject, no type prefix: `fix the overlay focus trap`.
- One logical change per commit. Squash before merge.
- PRs against `master`. CI runs the unit tests, `tsc --noEmit`, the Rust
  tests, clippy, and the reliable Playwright subset.

## License

By contributing you agree that your changes are released under the project's
  [LICENSE](LICENSE).
