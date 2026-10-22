# REA 6.5.0 security audit

This document is the operator-facing justification for the REA capability map in
`crates/pantheon-mcp/src/rea.rs`. It stands on its own: an operator reads it before
approving any REA tool. The authoritative inventory is
`vendor/rea_audit_split.json`, derived from REA's own `TOOL_EFFECTS` audit at
`src/contracts/toolEffects.ts` inside the REA source tree.

## Provenance

- Repository: `git+https://github.com/morluto/rea.git` (github.com/morluto/rea)
- npm package: `rea-agents`, mcpName `io.github.morluto/rea`
- Pinned version: `6.5.0`, license `MIT`
- Tarball: `https://registry.npmjs.org/rea-agents/-/rea-agents-6.5.0.tgz`
- Registry integrity (SRI, Subresource Integrity):
  `sha512-C6BG7LyrAdR74mSjertM6PrxJkcNZ2bfQNOJornPo9erDEp2tyPRVYPnV5Yodr+1RqewCQla68WulXlVnveD+A==`
- Registry SHA-1: `7ea5e6f669bc18c45dbf5dba653edaa1ada07ab3`
- Unpacked size: 25564765 bytes

### Trust model

Pantheon fetches the tarball at the pinned version, computes the SHA-256 of the
downloaded archive, and verifies it against the registry-published integrity value
before installing. Pantheon does not verify against a self-computed hash of whatever
arrives on the wire. It verifies against the registry's published `dist.integrity`
and `dist.shasum`. Install goes into a Pantheon-managed versioned local directory.
Pantheon executes that local artifact. It never runs `npx` at runtime, so nothing
from the registry reaches a process after the pin check passes.

## Risk model

Pantheon has two capability tiers for MCP servers:

- `FilesystemRead`: read-only static analysis. The tool reads the target file and
  returns data. It gains no privilege beyond what the agent already holds.
- `BinaryTrace`: anything that executes the target, mutates state, launches a
  process, or accesses the network. Default policy marks this `Approval`, so
  the run parks for a human. These external effects are not covered by a
  filesystem-read grant.

### Derivation rule

The bucket of a tool is derived from its effect flags:

- `READ` effect tier: the tool sets none of
  `{mutatesTarget, writesFilesystem, mutatesDevice, mayDiscardData, executesForeignCode}`.
  It maps to `FilesystemRead` only when it also has neither `launchesProcess`
  nor `accessesNetwork`; otherwise it maps to `BinaryTrace` and requires approval.
- `WRITE` (BinaryTrace): the tool sets at least one of
  `{mutatesTarget, writesFilesystem, mutatesDevice, mayDiscardData}` and does not
  set `executesForeignCode`.
- `EXEC` (BinaryTrace): the tool sets `executesForeignCode`, or sets
  `launchesProcess` together with `mutatesTarget`.

REA ships its own `TOOL_EFFECTS` audit and an `annotationsFromEffects` function that
emits `readOnlyHint` / `destructiveHint` annotations. Pantheon's split mirrors that
derivation so the two stay in lockstep. The READ/WRITE/EXEC buckets in
`rea_audit_split.json` are the output of applying this rule to every tool's flag
set. The runtime capability mapping adds the process/network approval rule above.

## Tool inventory: 110 tools

Counts: 43 READ, 42 WRITE, 25 EXEC.

The flag column lists the flags that are set for each tool. The derivation rule
above is applied to those flags to produce the bucket.

### READ effect tier (43 tools)

These set none of the WRITE or EXEC-driving flags. Entries with
`launchesProcess` or `accessesNetwork` still require approval; only entries
without either effect map to `FilesystemRead`. The listed flags are the ones
that remain true, mostly `launchesProcess`, `accessesNetwork`, and
`mutatesSession`.

| Tool | Flags set |
|---|---|
| binary_session | launchesProcess |
| compare_bundles | mutatesSession |
| decompile_jeb_item | accessesNetwork, mutatesSession |
| evaluate_reconstruction_coverage | |
| frida_session_status | |
| gdb_session_status | |
| get_evidence_bundle | |
| get_navigation_context | mutatesSession |
| goto_address | mutatesSession, changesUiState |
| import_evidence_bundle | mutatesSession |
| inspect_adb_client | launchesProcess, mutatesSession |
| inspect_adb_device | launchesProcess, accessesNetwork, mutatesSession |
| inspect_adb_display | launchesProcess, accessesNetwork, mutatesSession |
| inspect_adb_package | launchesProcess, accessesNetwork, mutatesSession |
| inspect_adb_window | launchesProcess, accessesNetwork, mutatesSession |
| inspect_address_context | mutatesSession |
| inspect_apktool_client | launchesProcess, mutatesSession |
| inspect_jeb_client | accessesNetwork, mutatesSession |
| inspect_native_api | mutatesSession |
| inspect_with_objdump | launchesProcess, mutatesSession |
| list_adb_devices | launchesProcess, accessesNetwork, mutatesSession |
| list_adb_directory | launchesProcess, accessesNetwork, mutatesSession |
| list_adb_features | launchesProcess, accessesNetwork, mutatesSession |
| list_adb_packages | launchesProcess, accessesNetwork, mutatesSession |
| list_adb_processes | launchesProcess, accessesNetwork, mutatesSession |
| list_adb_services | launchesProcess, accessesNetwork, mutatesSession |
| list_cutter_sessions | accessesNetwork |
| list_frida_devices | launchesProcess, accessesNetwork |
| list_frida_processes | launchesProcess, accessesNetwork |
| list_jeb_units | accessesNetwork, mutatesSession |
| list_unknowns | |
| open_binary | launchesProcess, mutatesSession |
| open_jeb_project | accessesNetwork, mutatesSession |
| read_adb_logcat | launchesProcess, accessesNetwork, mutatesSession |
| read_adb_setting | launchesProcess, accessesNetwork, mutatesSession |
| record_unknown | mutatesSession |
| resolve_adb_packages | launchesProcess, accessesNetwork, mutatesSession |
| rizin_debug_session_status | mutatesSession |
| start_gdb_session | launchesProcess, mutatesSession |
| trace_call_path | mutatesSession |
| trace_feature | mutatesSession |
| update_unknown | mutatesSession |
| verify_unknown_resolution | |

### WRITE (42 tools, BinaryTrace)

These set at least one of
`{mutatesTarget, writesFilesystem, mutatesDevice, mayDiscardData}` without
`executesForeignCode`. Most of the `inspect_*` and `decode_*` tools land here
because they write their results to the filesystem.

| Tool | Flags set |
|---|---|
| capture_adb_screen | writesFilesystem, launchesProcess, accessesNetwork, mutatesSession |
| close_binary | writesFilesystem, mayDiscardData, mutatesSession |
| collect_adb_bugreport | writesFilesystem, launchesProcess, accessesNetwork, mutatesSession |
| decode_android_resources | writesFilesystem, launchesProcess, mutatesSession |
| decode_signal_recording | writesFilesystem, launchesProcess, mutatesSession |
| export_evidence_bundle | writesFilesystem, mayDiscardData |
| export_web_scripts | writesFilesystem, mutatesSession |
| extract_artifact | writesFilesystem, launchesProcess, mutatesSession |
| extract_firmware | writesFilesystem, launchesProcess, mutatesSession |
| identify_flutter_build | writesFilesystem, mutatesSession |
| inspect_android_class | writesFilesystem, launchesProcess, mutatesSession |
| inspect_android_method | writesFilesystem, launchesProcess, mutatesSession |
| inspect_android_package | writesFilesystem, launchesProcess, mutatesSession |
| inspect_artifact | writesFilesystem, launchesProcess, mutatesSession |
| inspect_binary_layout | writesFilesystem, launchesProcess, mutatesSession |
| inspect_dart_aot | writesFilesystem, mutatesSession |
| inspect_evm_interface | writesFilesystem, launchesProcess, mutatesSession |
| inspect_firmware_regions | writesFilesystem, launchesProcess, mutatesSession |
| inspect_packet_capture | writesFilesystem, launchesProcess, mutatesSession |
| inspect_packet_fields | writesFilesystem, launchesProcess, mutatesSession |
| inspect_pcap_capture | writesFilesystem, launchesProcess, mutatesSession |
| inspect_pcap_client | writesFilesystem, launchesProcess, mutatesSession |
| inspect_recorded_crash | writesFilesystem, launchesProcess, mutatesSession |
| inspect_signal_decoders | writesFilesystem, launchesProcess, mutatesSession |
| inspect_sqlite_database | writesFilesystem, launchesProcess, mutatesSession |
| inspect_wasm_artifact | writesFilesystem, launchesProcess, mutatesSession |
| inspect_web_network_capture | writesFilesystem, launchesProcess, mutatesSession |
| observe_native_ui | writesFilesystem, launchesProcess, mutatesSession |
| observe_web_execution | mutatesTarget, mayDiscardData, accessesNetwork, mutatesSession |
| pull_adb_file | writesFilesystem, launchesProcess, accessesNetwork, mutatesSession |
| pull_adb_package | writesFilesystem, launchesProcess, accessesNetwork, mutatesSession |
| recover_javascript_sources | writesFilesystem, launchesProcess, mutatesSession |
| search_android_classes | writesFilesystem, launchesProcess, mutatesSession |
| set_address_name | mutatesTarget, mayDiscardData, mutatesSession |
| set_addresses_names | mutatesTarget, mayDiscardData, mutatesSession |
| set_bookmark | mutatesTarget, mayDiscardData, mutatesSession |
| set_comment | mutatesTarget, mayDiscardData, mutatesSession |
| set_inline_comment | mutatesTarget, mayDiscardData, mutatesSession |
| trace_android_references | writesFilesystem, launchesProcess, mutatesSession |
| trace_web_module_imports | writesFilesystem, launchesProcess, accessesNetwork, mutatesSession |
| trace_web_source_location | writesFilesystem, launchesProcess, mutatesSession |
| unset_bookmark | mutatesTarget, mayDiscardData, mutatesSession |

### EXEC (25 tools, BinaryTrace)

These set `executesForeignCode`, or `launchesProcess` together with
`mutatesTarget`. They run code in the target, drive live debugger sessions, or
mutate a connected device.

| Tool | Flags set |
|---|---|
| annotate_native_function | mutatesTarget, writesFilesystem, mayDiscardData, launchesProcess, mutatesSession |
| capture_browser_scenario | mutatesTarget, writesFilesystem, mayDiscardData, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| capture_electron_scenario | mutatesTarget, writesFilesystem, mayDiscardData, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| capture_native_ui_scenario | mutatesTarget, writesFilesystem, mayDiscardData, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| capture_process_scenario | writesFilesystem, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| close_frida_session | mutatesTarget, launchesProcess, accessesNetwork, mutatesSession |
| close_gdb_session | mutatesTarget, executesForeignCode, launchesProcess, mutatesSession |
| close_rizin_debug_session | mutatesTarget, executesForeignCode, launchesProcess, mutatesSession |
| cutter_command | mutatesTarget, writesFilesystem, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| execute_rizin_command | mutatesTarget, writesFilesystem, mayDiscardData, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| gdb_console | mutatesTarget, writesFilesystem, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| install_adb_package | mutatesTarget, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| instrument_with_frida | mutatesTarget, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| load_frida_script | mutatesTarget, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| observe_native_calls | writesFilesystem, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| push_adb_file | mutatesTarget, writesFilesystem, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| resume_frida_session | mutatesTarget, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession |
| rizin_debug_command | mutatesTarget, writesFilesystem, mayDiscardData, executesForeignCode, launchesProcess, accessesNetwork, mutatesSession, changesUiState |
| start_adb_activity | mutatesTarget, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| start_adb_app | mutatesTarget, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| start_frida_session | mutatesTarget, launchesProcess, accessesNetwork, mutatesSession |
| start_rizin_debug_session | mutatesTarget, executesForeignCode, launchesProcess, mutatesSession |
| stop_adb_app | mutatesTarget, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| uninstall_adb_package | mutatesTarget, mutatesDevice, launchesProcess, accessesNetwork, mutatesSession |
| unload_frida_script | mutatesTarget, launchesProcess, accessesNetwork, mutatesSession |

## Delta: 6.4.0 to 6.5.0

The 6.4.0 allowlist in `rea.rs` covered 45 names (35 READ, 10 EXEC). The 6.5.0
inventory has 110 names. The delta, as recorded in `rea_audit_split.json`:

- 38 new READ-only tools (`newRead` in the JSON).
- 54 new WRITE/EXEC tools (`newTrace` in the JSON, 31 WRITE plus 23 EXEC).
- 27 names from 6.4.0 no longer present in 6.5.0 (`gone64`).

The 18 names that were in the 6.4.0 allowlist and remain in 6.5.0:
`capture_electron_scenario`, `capture_process_scenario`, `compare_bundles`,
`extract_artifact`, `extract_firmware`, `get_navigation_context`,
`inspect_android_class`, `inspect_android_method`, `inspect_android_package`,
`inspect_binary_layout`, `inspect_evm_interface`, `inspect_firmware_regions`,
`inspect_native_api`, `inspect_wasm_artifact`, `search_android_classes`,
`trace_android_references`, `trace_call_path`, `trace_feature`.

Of these, three moved buckets between versions. `trace_call_path`,
`trace_feature`, and `trace_android_references` were in the 6.4.0 tracing
allowlist. In 6.5.0, `trace_call_path` and `trace_feature` set no
target-mutating, filesystem-writing, or code-executing flags, so they are
READ. `trace_android_references` is now WRITE: it gained the
`writesFilesystem` flag. The other 15 keep the same capability tier as
before, though several gained new flags: `inspect_binary_layout`,
`inspect_evm_interface`, `inspect_wasm_artifact`, `extract_firmware`,
`inspect_android_class`, `inspect_android_method`, `inspect_android_package`
were READ in 6.4.0 and are now WRITE, because they gained the
`writesFilesystem` flag in 6.5.0. `search_android_classes` moved the same
way, from READ to WRITE. `capture_electron_scenario` and
`capture_process_scenario` remain EXEC. `get_navigation_context` stays READ;
its 6.5.0 flag set is just `mutatesSession`. The operator should treat the
6.5.0 bucket as the new source of truth and re-approve any tool whose bucket
or flag set changed.

The 27 `gone64` names are:
`binary_overview`, `analyze_function`, `batch_decompile`, `get_call_graph`,
`find_xrefs_to_name`, `inspect_native_dispatch_metadata`, `get_objc_classes`,
`get_objc_protocols`, `analyze_swift_types`, `inspect_go_binary`,
`analyze_web_bundle`, `inspect_web_page`, `analyze_javascript_application`,
`inspect_electron_page`, `list_electron_targets`, `list_browser_targets`,
`inspect_analysis_view`, `compare_artifacts`, `compare_functions`,
`compare_process_captures`, `compare_web_captures`, `compare_web_screenshots`,
`trace_native_values`, `trace_native_ui_action`, `capture_web_screenshot`,
`observe_web_session`, `reconcile_javascript_runtime`.

A gone name is safe under the fail-closed default. An unlisted tool falls back to
the server-level `BinaryTrace` capability and parks for approval. The operator
just gets a prompt. The audit must re-map these names if the operator needs to
know what replaced them, so the approval record can reference the successor tool.

The runtime applies this per-tool map only when the server reports the audited
implementation version `6.5.0` during the MCP handshake. A missing or different
version uses the server-level `BinaryTrace` fallback for every tool. The reported
version is combined with the existing server content-hash approval; the map is
never selected by server name alone.

## New high-risk families in 6.5.0

Three tool families appear for the first time in 6.5.0 that the 6.4.0 map did not
know about. They are worse than plain tracing because they run arbitrary commands
against a live target, not against a static file.

- Live debugger sessions: `gdb_*`, `rizin_*`, `cutter_*`, `frida_*`. These
  attach to a running process and drive a debugger or instrumentation harness.
  `gdb_console`, `execute_rizin_command`, `rizin_debug_command`,
  `cutter_command`, `load_frida_script`, `instrument_with_frida`, and
  `resume_frida_session` all set `executesForeignCode`. The target is a live
  process, not a file on disk.
- ADB device mutation: `install_adb_package`, `uninstall_adb_package`,
  `start_adb_app`, `stop_adb_app`, `start_adb_activity`, `push_adb_file`.
  These set `mutatesDevice` together with `mutatesTarget` and `launchesProcess`.
  They change the state of a connected Android device: they install and remove
  packages and start and stop applications.
- Write-mutates-target annotations: `set_address_name`,
  `set_addresses_names`, `set_bookmark`, `set_comment`, `set_inline_comment`,
  `unset_bookmark`, and `observe_web_execution`. These set `mutatesTarget` and
  `mayDiscardData`. They overwrite prior annotations in the target file, so a
  second run can destroy the first run's results.

## Approval boundary and how pantheon closes it

A version pin does not verify the registry artifact or its transitive dependencies.
The pin says "use the 6.5.0 line". It does not say "this exact tarball, with these
exact dependencies". Pantheon therefore binds approval to four things together:

1. The resolved artifact hash. The SHA-256 of the downloaded tarball, verified
   against the registry-published `dist.integrity` and `dist.shasum` before install.
2. The execution configuration. How the server is launched, what tools it is
   told to expose, and what environment it runs in.
3. The relevant runtime dependencies. The set of packages the artifact actually
   loads, checked against the dependency manifest.
4. A live `tools/list` gate. Pantheon runs the MCP `tools/list` call and blocks any
   tool that is not in the approval record. If the list changes between
   approvals, the change requires a fresh human approval before the new tool
   runs.

The gate is the enforcement point. The hash, configuration, and dependency checks
narrow what "approval" means. The gate decides, at call time, whether the specific
tool is in the approved set.

## Node runtime requirement

The `engines` field in `rea-agents@6.5.0` is:

```
"node": "^22.19.0 || ^24.11.0 || >=26.0.0"
```

Pantheon's policy for the Node runtime is detect-or-install:

- Check for a supported Node version on the system.
- If none is present, explain what is missing to the operator.
- Provide trusted install instructions for the missing version.
- Never silently install a Node runtime as a side effect of tool approval.

Weight on a reference Linux box: the Node binary is about 143 MB. The full Node
tree including the REA package and its dependencies is about 229 MB.

## Licensing

REA is MIT. Retaining the vendored source in-repo is not a licensing obligation.
The audit document, the version pin, and the integrity values are what Pantheon
keeps in-repo as the operator-facing reference.
