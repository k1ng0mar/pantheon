# File icons

The Files tab, the `@` [file mention](file-mentions.md) menu and the
[changed-file tree](turn-changes.md) show file and folder icons from
[Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme)
(the `material-icon-theme` npm package, MIT), matched the way VS Code does: exact
file name, then the longest extension (`.d.ts` before `.ts`), then the default
file icon. Folders match by name (`src`, `docs`, `json`, …) or use the default
folder. The light theme uses the package's light variants where they exist.

`src/file-icons.ts` reads the package's prebuilt `dist/material-icons.json` and
points `<img>` tags at its SVGs. `src/dom.ts` loads that module with a dynamic
import so the manifest stays out of the startup bundle; plain glyphs show until
it arrives, then `main.ts` re-renders once, and again when the theme changes.
Mention rows with their own icon (GitHub issues and pull requests) keep it.

`tests/file-icons.test.mjs` covers the matching, light variants, names such as
`constructor` that collide with `Object.prototype`, and that every resolved icon
exists on disk.
