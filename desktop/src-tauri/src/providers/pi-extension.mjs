// Pantheon app tools that only read state stay available in Plan mode; every other app tool is blocked.
const PLAN_APP_TOOLS = new Set([
  'pantheon_title', 'pantheon_team_directory', 'pantheon_read_team_mailbox',
  'pantheon_list_tasks', 'pantheon_read_task', 'pantheon_search_tasks', 'pantheon_list_projects',
  'pantheon_list_workers', 'pantheon_read_worker', 'pantheon_list_subagents', 'pantheon_list_queue',
  'pantheon_list_local_servers', 'pantheon_list_moodboards', 'pantheon_read_moodboard', 'pantheon_read_moodboard_image',
  'pantheon_vm_list', 'pantheon_vm_screenshot', 'pantheon_simulator_devices', 'pantheon_simulator_describe',
  'pantheon_simulator_screenshot', 'pantheon_jev_read', 'pantheon_jev_list', 'pantheon_jev_screenshot',
  'pantheon_jev_assessments', 'pantheon_bot_recall', 'pantheon_bot_list_schedules', 'pantheon_code_search',
]);

export default async function (pi) {
  const config = JSON.parse(process.env.PANTHEON_PI_BRIDGE);
  // The bearer token must not reach the commands Pi runs (they inherit this environment).
  delete process.env.PANTHEON_PI_BRIDGE;
  const request = async (method, params, signal) => {
    const response = await fetch(config.url, {
      method: 'POST',
      signal,
      headers: { 'Content-Type': 'application/json', Authorization: config.authorization },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    });
    if (!response.ok) throw new Error('Pantheon tools connection failed.');
    const result = await response.json();
    if (result.error) throw new Error(result.error.message);
    return result.result;
  };
  const tools = config.url ? (await request('tools/list', {})).tools : [];
  const names = new Set(tools.map(tool => tool.name));
  for (const tool of tools) {
    pi.registerTool({
      name: tool.name,
      label: tool.name,
      // omp hides extension tools behind discovery unless they are essential; Pi ignores this.
      loadMode: 'essential',
      description: tool.description,
      parameters: tool.inputSchema,
      async execute(_id, args, signal) {
        const result = await request('tools/call', { name: tool.name, arguments: args }, signal);
        if (result.isError) throw new Error(result.content.filter(item => item.type === 'text').map(item => item.text).join('\n'));
        return result;
      },
    });
  }
  pi.on('session_start', (_event, ctx) => ctx.ui.setStatus('pantheon-bridge', 'ready'));
  pi.on('tool_call', async (event, ctx) => {
    const app = names.has(event.toolName);
    if (['read', 'grep', 'find', 'ls'].includes(event.toolName) || (app && PLAN_APP_TOOLS.has(event.toolName))) return;
    if (config.mode === 'plan') return { block: true, reason: 'This tool is unavailable in Plan mode.' };
    if (app) return;
    if (config.mode === 'full') return;
    if (config.mode === 'auto' && ['edit', 'write'].includes(event.toolName)) return;
    try {
      if (await ctx.ui.confirm(`Allow ${event.toolName}?`, JSON.stringify(event.input).slice(0, 4000))) return;
    } catch {}
    return { block: true, reason: 'Declined by the user.' };
  });
}
