import type { Mode, ProviderId } from './types';

export interface ApprovalMode { id: Mode; label: string; description: string; icon: string; disabled?: boolean; }

/**
 * The access modes Pantheon's ACP server actually offers: `build` and `plan`.
 *
 * Pantheon has no separate edit-approval mode, so "Accept edits" is not a
 * policy it can be put into and is no longer offered. Full access is the one
 * mode that lives on this side of the wire: the app answers every
 * `session/request_permission` with allow. Everything else is Pantheon's own
 * policy, and it still asks you when a tool needs permission.
 */
const MODES: ApprovalMode[] = [
  { id: 'ask', label: 'Standard', icon: 'check', description: 'Work in this project. Pantheon asks you before it runs a tool its policy requires approval for.' },
  { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only: Pantheon investigates and proposes a plan; edits and commands are declined.' },
  { id: 'full', label: 'Full access', icon: 'unlock', description: 'This app approves every permission Pantheon asks for, including outside this project. Pantheon’s own policy can still refuse an action.' },
];

/** Modes an older build could save that this build cannot run. */
const RETIRED: Record<string, { label: string; description: string }> = {
  auto: { label: 'Accept edits', description: 'Pantheon has no edit-approval mode. Choose Standard, Plan or Full access.' },
  review: { label: 'Approve for me', description: 'Pantheon has no agent-reviewed approvals. Choose Standard, Plan or Full access.' },
};

/** Labels describe the agent's own policy, not a simulated approval layer. */
export function approvalModes(provider: ProviderId, current?: Mode): ApprovalMode[] {
  void provider;
  const retired = current ? RETIRED[current] : undefined;
  return retired
    ? [...MODES, { id: current!, label: retired.label, icon: 'spark', disabled: true, description: retired.description }]
    : MODES;
}

export function approvalMode(provider: ProviderId, mode: Mode): ApprovalMode {
  const modes = approvalModes(provider, mode);
  return modes.find(item => item.id === mode) ?? modes[0];
}
