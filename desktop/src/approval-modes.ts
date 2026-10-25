import type { Mode, ProviderId } from './types';

export interface ApprovalMode { id: Mode; label: string; description: string; icon: string; disabled?: boolean; }

const CLAUDE_FAMILY: ProviderId[] = ['claude', 'glm', 'mimo'];

/** Labels describe the native policy for each harness, not a simulated approval layer. */
export function approvalModes(provider: ProviderId, current?: Mode): ApprovalMode[] {
  // omp is a Pi fork driven through the same bridge, so it shares Pi's policy.
  const pi = provider === 'pi' || provider === 'omp' ? (provider === 'omp' ? 'omp' : 'Pi') : '';
  const standard: ApprovalMode = { id: 'ask', label: 'Standard', icon: 'check', description: provider === 'codex'
    ? 'Work in this project. Ask you when an action needs extra permission.'
    : provider === 'github-copilot'
    ? 'Use Copilot Agent mode with Allow All off. Pantheon asks before permission requests.'
    : provider === 'grok'
    ? 'Grok asks before edits and commands, whatever its own config says.'
    : pi
    ? `${pi} runs without its own approvals; Pantheon asks before edits, commands and other non-read tools.`
    : provider === 'deepseek'
    ? 'DeepSeek edits and runs commands inside this project freely; it asks you before going outside it.'
    : provider === 'cline'
    ? 'Cline acts; reads and searches run, Pantheon asks before edits, commands and other tools.'
    : provider === 'kilo'
    ? 'Kilo runs read-only tools; Pantheon asks before edits, commands and other tools.'
    : provider === 'muse'
    ? 'Muse asks before actions its rules do not allow. Its own reviewer may approve low-risk ones.'
    : 'Use the harness’s normal rules. Ask you when approval is needed.' };
  const full: ApprovalMode = { id: 'full', label: 'Full access', icon: 'unlock', description: provider === 'antigravity'
    ? 'Automatically approve permission requests. Server restrictions still apply.'
    : provider === 'codex'
    ? 'Skip approval prompts and sandbox limits. Allow file and network access.'
    : provider === 'grok'
    ? 'Always approve: Grok runs every tool without asking (its --always-approve).'
    : pi
    ? `${pi} runs every tool without asking.`
    : provider === 'deepseek'
    ? 'No sandbox: DeepSeek runs every tool without asking.'
    : provider === 'cline'
    ? 'Cline acts with its auto-approve on: every tool runs without asking.'
    : provider === 'muse'
    ? 'No approvals and no sandbox: Muse runs every tool without asking.'
    : provider === 'github-copilot'
    ? 'Use Copilot Agent mode with Allow All on. Copilot automatically approves tool, path, and URL requests.'
    : 'Run tools without approval prompts, including outside this project.' };
  // Agent-reviewed approvals, as bots offer. Codex's own is its Approve for me (`auto`).
  const review: ApprovalMode[] = CLAUDE_FAMILY.includes(provider)
    ? [{ id: 'review', label: 'Approve for me', icon: 'spark', description: 'Claude Code’s auto mode: its safety reviewer approves routine actions and blocks risky ones. Needs a recent Claude Code.' }]
    // A task switched here keeps its mode; show why it cannot run rather than another policy.
    : current !== 'review' ? []
    : [{ id: 'review', label: 'Approve for me', icon: 'spark', disabled: true, description: 'This harness has no agent-reviewed approvals. Choose another mode, or use Claude Code (or Codex’s Approve for me).' }];
  if (provider === 'github-copilot') return [standard,
    { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Use Copilot Plan mode. Read, search, fetch, and think requests are allowed; other permission requests are declined.' },
    ...review, full];
  return [standard, provider === 'muse'
    ? { id: 'auto', label: 'Accept edits', icon: 'edit', description: 'Work freely inside the sandbox. Muse asks only to leave it.' }
    : provider === 'deepseek'
    ? { id: 'auto', label: 'Accept edits', icon: 'edit', description: 'Like Standard, and edits outside this project are approved too. Other actions still ask.' }
    : ['claude', 'glm', 'mimo', 'opencode', 'grok', 'pi', 'omp', 'commandcode', 'cline', 'kilo'].includes(provider)
    ? { id: 'auto', label: 'Accept edits', icon: 'edit', description: 'Allow file edits automatically. Other actions may still ask you.' }
    : provider === 'kimi'
    ? { id: 'auto', label: 'Ask when needed', icon: 'spark', description: 'Allow routine tools automatically. Ask you about sensitive actions.' }
    : provider === 'codex' ? { id: 'auto', label: 'Approve for me', icon: 'spark', description: 'Let Codex review approval requests. Keep the workspace sandbox.' }
    : { id: 'auto', label: 'Accept edits', icon: 'edit', description: 'Automatically approve edit requests offered by the server.' },
  ...review,
  provider === 'codex'
    ? { id: 'plan', label: 'Read only', icon: 'plan', description: 'Inspect the project. Block writes and permission escalation.' }
    : provider === 'grok'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only: Grok investigates and proposes a plan; edits and commands are declined.' }
    : pi
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: `Read-only: ${pi} may use read-only tools; everything else is declined.` }
    : provider === 'deepseek'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only sandbox: DeepSeek cannot write files and proposes a plan; requests to escalate are declined.' }
    : provider === 'cline'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Cline’s Plan mode: it explores, fetches and proposes a plan; file edits are declined, commands still ask.' }
    : provider === 'kilo'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only: Kilo investigates and plans; edits, commands and other state-changing tools are declined.' }
    : provider === 'muse'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only: Muse cannot write files or run shell commands, and proposes a plan.' }
    : provider === 'commandcode'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Read-only: Command Code researches and plans; edits and commands are declined.' }
    : CLAUDE_FAMILY.includes(provider)
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Explore and plan: reads and web lookups run, even outside this project; edits wait for implementation and commands ask you.' }
    : provider === 'opencode'
    ? { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Explore and plan: reads and web lookups run; reading outside this project asks you; edits and commands are declined.' }
    : { id: 'plan', label: 'Plan mode', icon: 'plan', description: 'Explore and plan changes before implementation.' }, full];
}

export function approvalMode(provider: ProviderId, mode: Mode): ApprovalMode {
  const modes = approvalModes(provider, mode);
  return modes.find(item => item.id === mode) ?? modes[0];
}
