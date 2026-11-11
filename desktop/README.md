# Pantheon Desktop

Tauri 2 workspace for Pantheon. Rust, SQLite, TypeScript, OS webview.

New tasks default to the Pantheon harness (`pantheon serve acp`). Other installed CLIs still show up through the same discovery layer.

## Build

Node ≥ 20, Rust stable, Tauri 2 system deps (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`).

```sh
npm install
npm run tauri dev
```

Needs a Pantheon binary on PATH, at `~/projects/pantheon/target/release/pantheon`, or at `~/.local/bin/pantheon`.

`npm run build` produces `.deb` / `.rpm` / `.AppImage`.

## License

See `LICENSE`.
