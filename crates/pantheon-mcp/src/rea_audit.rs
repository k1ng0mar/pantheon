//! REA 6.5.0 tool capability map, generated from the vendor effect audit.
//!
//! Every row stores the full ten-flag effect vector as a bitmask plus the
//! tier derived from it. [`reaudit`] recomputes the tier from the flags and
//! fails on any disagreement. That is the drift check the live tools/list
//! gate calls before trusting the map. Effect bit order:
//!
//! | bit | flag               |
//! |---|--------------------|
//! | 0   | mutatesTarget      |
//! | 1   | writesFilesystem   |
//! | 2   | mutatesDevice      |
//! | 3   | mayDiscardData     |
//! | 4   | executesForeignCode|
//! | 5   | launchesProcess    |
//! | 6   | accessesNetwork    |
//! | 7   | mutatesSession     |
//! | 8   | changesUiState     |
//! | 9   | idempotent         |
//!
//! Source: assets/rea_audit_split.json, extracted from REA's own
//! toolEffects.ts (the contract that derives readOnlyHint/destructiveHint).
//! The buckets reconcile exactly: 43 Read, 42 Write, 25 Exec, 110 tools.

use crate::rea::REA_SERVER_NAME;
use pantheon_api::capability::Capability;
use std::collections::HashMap;

/// REA release the embedded inventory was audited against.
pub const REA_PINNED_VERSION: &str = "6.5.0";

/// SHA-256 of the exact rea-agents-6.5.0.tgz this inventory came from.
pub const REA_TARBALL_SHA256: &str =
    "531717a06e92ca651aab52691190f916fac044272c01f875e22c5e293218e699";

/// SRI value (base64 SHA-512) of the same tarball, kept for reference.
pub const REA_TARBALL_SRI: &str = "sha512-C6BG7LyrAdR74mSjertM6PrxJkcNZ2bfQNOJornPo9erDEp2tyPRVYPnV5Yodr+1RqewCQla68WulXlVnveD+A==";

/// The three risk tiers.
///
/// Read: static analysis. The tool reads the target and returns analysis;
/// no new privilege beyond what the agent already has.
/// Write: mutates the target file, the REA session, or the filesystem.
/// Exec: runs the target or executes foreign code in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaTier {
    Read,
    Write,
    Exec,
}

/// One audited REA tool: name, effect bitmask, and derived tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaTool {
    pub name: &'static str,
    pub mask: u32,
    pub tier: ReaTier,
}

const FLAG_MUTATES_TARGET: u32 = 0x1;
const FLAG_WRITES_FILESYSTEM: u32 = 0x2;
const FLAG_MUTATES_DEVICE: u32 = 0x4;
const FLAG_MAY_DISCARD_DATA: u32 = 0x8;
const FLAG_EXECUTES_FOREIGN_CODE: u32 = 0x10;
const FLAG_LAUNCHES_PROCESS: u32 = 0x20;
const FLAG_ACCESSES_NETWORK: u32 = 0x40;
const FLAG_MUTATES_SESSION: u32 = 0x80;
const FLAG_CHANGES_UI_STATE: u32 = 0x100;
const FLAG_IDEMPOTENT: u32 = 0x200;

/// Decode an effect bitmask back into the ten individual flags, in the
/// order listed at the top of this file. Exposed so a future audit can
/// re-derive the tier from the flags without re-fetching the REA source.
pub fn mask_flags(mask: u32) -> [bool; 10] {
    [
        mask & FLAG_MUTATES_TARGET != 0,
        mask & FLAG_WRITES_FILESYSTEM != 0,
        mask & FLAG_MUTATES_DEVICE != 0,
        mask & FLAG_MAY_DISCARD_DATA != 0,
        mask & FLAG_EXECUTES_FOREIGN_CODE != 0,
        mask & FLAG_LAUNCHES_PROCESS != 0,
        mask & FLAG_ACCESSES_NETWORK != 0,
        mask & FLAG_MUTATES_SESSION != 0,
        mask & FLAG_CHANGES_UI_STATE != 0,
        mask & FLAG_IDEMPOTENT != 0,
    ]
}

/// Derive the tier from an effect bitmask.
///
/// Exec if the tool runs foreign code, or launches a process into a target
/// it mutates. Write if it mutates the target, filesystem, or device, or
/// can discard data. Read otherwise.
pub fn tier_from_mask(mask: u32) -> ReaTier {
    let exec = mask & FLAG_EXECUTES_FOREIGN_CODE != 0
        || (mask & FLAG_LAUNCHES_PROCESS != 0) && (mask & FLAG_MUTATES_TARGET != 0);
    let write = mask & FLAG_MUTATES_TARGET != 0
        || mask & FLAG_WRITES_FILESYSTEM != 0
        || mask & FLAG_MUTATES_DEVICE != 0
        || mask & FLAG_MAY_DISCARD_DATA != 0;
    if exec {
        ReaTier::Exec
    } else if write {
        ReaTier::Write
    } else {
        ReaTier::Read
    }
}

/// The full REA 6.5.0 inventory: 110 tools with effect vectors and tiers.
const REA_TOOLS: &[ReaTool] = &[
    ReaTool {
        name: "annotate_native_function",
        mask: 0x2ab,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "binary_session",
        mask: 0x220,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "capture_adb_screen",
        mask: 0xe2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "capture_browser_scenario",
        mask: 0x1fb,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "capture_electron_scenario",
        mask: 0x1fb,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "capture_native_ui_scenario",
        mask: 0x1fb,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "capture_process_scenario",
        mask: 0xf2,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "close_binary",
        mask: 0x8a,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "close_frida_session",
        mask: 0xe1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "close_gdb_session",
        mask: 0xb1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "close_rizin_debug_session",
        mask: 0x2b1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "collect_adb_bugreport",
        mask: 0xe2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "compare_bundles",
        mask: 0x280,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "cutter_command",
        mask: 0x1f3,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "decode_android_resources",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "decode_signal_recording",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "decompile_jeb_item",
        mask: 0x2c0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "evaluate_reconstruction_coverage",
        mask: 0x200,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "execute_rizin_command",
        mask: 0xfb,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "export_evidence_bundle",
        mask: 0x20a,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "export_web_scripts",
        mask: 0x82,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "extract_artifact",
        mask: 0xa2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "extract_firmware",
        mask: 0xa2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "frida_session_status",
        mask: 0x200,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "gdb_console",
        mask: 0xf3,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "gdb_session_status",
        mask: 0x200,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "get_evidence_bundle",
        mask: 0x200,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "get_navigation_context",
        mask: 0x280,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "goto_address",
        mask: 0x380,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "identify_flutter_build",
        mask: 0x282,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "import_evidence_bundle",
        mask: 0x280,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_adb_client",
        mask: 0x2a0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_adb_device",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_adb_display",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_adb_package",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_adb_window",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_address_context",
        mask: 0x280,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_android_class",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_android_method",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_android_package",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_apktool_client",
        mask: 0x2a0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_artifact",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_binary_layout",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_dart_aot",
        mask: 0x282,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_evm_interface",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_firmware_regions",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_jeb_client",
        mask: 0x2c0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_native_api",
        mask: 0x80,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "inspect_packet_capture",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_packet_fields",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_pcap_capture",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_pcap_client",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_recorded_crash",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_signal_decoders",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_sqlite_database",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_wasm_artifact",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_web_network_capture",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "inspect_with_objdump",
        mask: 0x2a0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "install_adb_package",
        mask: 0xe5,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "instrument_with_frida",
        mask: 0xf1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "list_adb_devices",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_adb_directory",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_adb_features",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_adb_packages",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_adb_processes",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_adb_services",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_cutter_sessions",
        mask: 0x240,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_frida_devices",
        mask: 0x260,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_frida_processes",
        mask: 0x260,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_jeb_units",
        mask: 0x2c0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "list_unknowns",
        mask: 0x200,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "load_frida_script",
        mask: 0xf1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "observe_native_calls",
        mask: 0x1f2,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "observe_native_ui",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "observe_web_execution",
        mask: 0xc9,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "open_binary",
        mask: 0x2a0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "open_jeb_project",
        mask: 0xc0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "pull_adb_file",
        mask: 0xe2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "pull_adb_package",
        mask: 0xe2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "push_adb_file",
        mask: 0xe7,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "read_adb_logcat",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "read_adb_setting",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "record_unknown",
        mask: 0x80,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "recover_javascript_sources",
        mask: 0xa2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "resolve_adb_packages",
        mask: 0x2e0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "resume_frida_session",
        mask: 0xf1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "rizin_debug_command",
        mask: 0x1fb,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "rizin_debug_session_status",
        mask: 0x280,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "search_android_classes",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "set_address_name",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "set_addresses_names",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "set_bookmark",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "set_comment",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "set_inline_comment",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "start_adb_activity",
        mask: 0xe5,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "start_adb_app",
        mask: 0xe5,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "start_frida_session",
        mask: 0xe1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "start_gdb_session",
        mask: 0xa0,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "start_rizin_debug_session",
        mask: 0xb1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "stop_adb_app",
        mask: 0xe5,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "trace_android_references",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "trace_call_path",
        mask: 0x80,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "trace_feature",
        mask: 0x80,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "trace_web_module_imports",
        mask: 0x2e2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "trace_web_source_location",
        mask: 0x2a2,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "uninstall_adb_package",
        mask: 0xe5,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "unload_frida_script",
        mask: 0xe1,
        tier: ReaTier::Exec,
    },
    ReaTool {
        name: "unset_bookmark",
        mask: 0x289,
        tier: ReaTier::Write,
    },
    ReaTool {
        name: "update_unknown",
        mask: 0x80,
        tier: ReaTier::Read,
    },
    ReaTool {
        name: "verify_unknown_resolution",
        mask: 0x200,
        tier: ReaTier::Read,
    },
];

/// Per-tool capability map, keyed by `(server_name, tool_name)` like the
/// old hand-maintained allowlist in [`crate::rea`]. Read-tier tools are
/// [`Capability::FilesystemRead`] ONLY when they neither launch a process
/// nor access the network (the rule [`crate::rea`] documents): 26
/// Read-tier tools in this inventory launch processes and/or touch the
/// network (e.g. `start_gdb_session`, `inspect_adb_device`,
/// `list_cutter_sessions`), and those map to [`Capability::BinaryTrace`]
/// so default policies park them for approval instead of letting a
/// process-launching "read" tool ride the permissive FilesystemRead
/// capability. Write and Exec-tier tools are [`Capability::BinaryTrace`]
/// as well. Tools absent from this map fall back to
/// [`crate::rea::rea_server_capability`].
pub fn rea_tool_capabilities() -> HashMap<(String, String), Capability> {
    REA_TOOLS
        .iter()
        .map(|t| {
            // The audit tier alone does not decide the capability:
            // a Read-tier tool that spawns a process or touches the
            // network is an external effect, not a static read.
            let external = t.mask & FLAG_LAUNCHES_PROCESS != 0
                || t.mask & FLAG_ACCESSES_NETWORK != 0;
            let cap = match t.tier {
                ReaTier::Read if !external => Capability::FilesystemRead,
                _ => Capability::BinaryTrace,
            };
            ((REA_SERVER_NAME.to_string(), t.name.to_string()), cap)
        })
        .collect()
}

/// Drift check: recompute every tool's tier from its stored effect flags
/// and fail if the stored tier disagrees. The live tools/list gate calls
/// this at handshake; if REA renames a tool, the unknown tool surfaces on
/// its own, and this invariant keeps the embedded map self-consistent.
pub fn reaudit() -> Result<(), String> {
    for tool in REA_TOOLS {
        let derived = tier_from_mask(tool.mask);
        if derived != tool.tier {
            return Err(format!(
                "tool {}: stored tier {:?} disagrees with derived tier {:?} (mask {:#06x})",
                tool.name, tool.tier, derived, tool.mask
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_has_exactly_110_entries() {
        assert_eq!(REA_TOOLS.len(), 110);
        let m = rea_tool_capabilities();
        assert_eq!(m.len(), 110);
    }

    #[test]
    fn tiers_are_derivable_from_flags() {
        assert_eq!(tier_from_mask(FLAG_EXECUTES_FOREIGN_CODE), ReaTier::Exec);
        assert_eq!(
            tier_from_mask(FLAG_LAUNCHES_PROCESS | FLAG_MUTATES_TARGET),
            ReaTier::Exec
        );
        assert_eq!(tier_from_mask(FLAG_LAUNCHES_PROCESS), ReaTier::Read);
        assert_eq!(tier_from_mask(FLAG_WRITES_FILESYSTEM), ReaTier::Write);
        assert_eq!(
            tier_from_mask(FLAG_MUTATES_TARGET | FLAG_MUTATES_SESSION),
            ReaTier::Write
        );
        assert_eq!(
            tier_from_mask(FLAG_ACCESSES_NETWORK | FLAG_MUTATES_SESSION),
            ReaTier::Read
        );
        assert_eq!(tier_from_mask(0), ReaTier::Read);
    }

    #[test]
    fn read_tools_are_filesystem_read() {
        // Only non-external Read tools are FilesystemRead: a Read
        // tool that launches a process or touches the network parks
        // as BinaryTrace (26 tools in this inventory).
        let m = rea_tool_capabilities();
        let mut external_read = 0;
        for t in REA_TOOLS {
            if t.tier == ReaTier::Read {
                let key = (REA_SERVER_NAME.to_string(), t.name.to_string());
                let external = t.mask & FLAG_LAUNCHES_PROCESS != 0
                    || t.mask & FLAG_ACCESSES_NETWORK != 0;
                if external {
                    external_read += 1;
                }
                assert_eq!(
                    m.get(&key),
                    Some(if external {
                        &Capability::BinaryTrace
                    } else {
                        &Capability::FilesystemRead
                    }),
                    "tool {} {:#06x}",
                    t.name,
                    t.mask
                );
            }
        }
        assert_eq!(external_read, 26);
        for tool in ["start_gdb_session", "inspect_adb_device", "list_cutter_sessions"] {
            let key = (REA_SERVER_NAME.to_string(), tool.to_string());
            assert_eq!(m.get(&key), Some(&Capability::BinaryTrace), "tool {tool}");
        }
        for tool in ["goto_address", "list_unknowns", "trace_call_path"] {
            let key = (REA_SERVER_NAME.to_string(), tool.to_string());
            assert_eq!(
                m.get(&key),
                Some(&Capability::FilesystemRead),
                "tool {tool}"
            );
        }
    }

    #[test]
    fn write_and_exec_tools_are_binary_trace() {
        let m = rea_tool_capabilities();
        for t in REA_TOOLS {
            if t.tier != ReaTier::Read {
                let key = (REA_SERVER_NAME.to_string(), t.name.to_string());
                assert_eq!(
                    m.get(&key),
                    Some(&Capability::BinaryTrace),
                    "tool {} {:#06x}",
                    t.name,
                    t.mask
                );
            }
        }
    }

    #[test]
    fn known_exec_tools_are_present() {
        let m = rea_tool_capabilities();
        for tool in ["gdb_console", "instrument_with_frida", "cutter_command"] {
            let key = (REA_SERVER_NAME.to_string(), tool.to_string());
            assert_eq!(m.get(&key), Some(&Capability::BinaryTrace), "tool {tool}");
        }
    }

    #[test]
    fn mask_flags_round_trips() {
        for t in REA_TOOLS {
            let flags = mask_flags(t.mask);
            let rebuilt: u32 = flags
                .iter()
                .enumerate()
                .fold(0, |acc, (i, b)| acc | (*b as u32) << i);
            assert_eq!(rebuilt, t.mask, "tool {} {:#06x}", t.name, t.mask);
        }
    }

    #[test]
    fn reaudit_passes_on_embedded_data() {
        assert!(reaudit().is_ok());
    }
}
