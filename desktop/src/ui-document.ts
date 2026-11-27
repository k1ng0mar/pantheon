/* Native `_pantheon/uiDocument` payloads. The server already sends the readable
 * version as markdown; this module only adds the structured view for clients
 * that opt in. Actions stay local: they fill the composer, they never execute. */

const LIMITS = { source: 64 * 1024, depth: 12, nodes: 120 };

interface Raw {
  type?: string;
  id?: string;
  [key: string]: unknown;
}

export interface NativeUiDocument {
  version: number;
  id: string;
  root: Raw;
}

export interface NativeUiEvent {
  uiId: string;
  revision: number;
  document: NativeUiDocument;
}

const obj = (value: unknown): Raw => value && typeof value === 'object' && !Array.isArray(value) ? value as Raw : {};
const text = (value: unknown): string => typeof value === 'string' ? value : '';
const list = (value: unknown): unknown[] => Array.isArray(value) ? value.slice(0, 200) : [];

/** Map Pantheon's wire components onto the renderer's view vocabulary. */
function view(node: unknown, depth: number, count: { value: number }): Raw | null {
  if (depth > LIMITS.depth || ++count.value > LIMITS.nodes) return null;
  const raw = obj(node);
  const type = text(raw.type);
  switch (type) {
    case 'markdown': return { type: 'markdown', text: text(raw.text ?? raw.body).slice(0, 4000) };
    case 'metrics': return { type: 'stats', items: list(raw.items).map(entry => {
      const item = obj(entry);
      return { label: text(item.label), value: text(item.value), delta: text(item.delta) };
    }).filter(item => item.label && item.value) };
    case 'table': return {
      type: 'table',
      columns: list(raw.headers).map(header => ({ label: text(header) })),
      rows: list(raw.rows).map(row => list(row).map(cell => text(cell))),
    };
    case 'chart': return {
      type: 'chart', chart: raw.kind ?? 'bar',
      labels: list(raw.labels).map(label => text(label)),
      series: list(raw.series).map(entry => {
        const series = obj(entry);
        return { name: text(series.name), values: list(series.values).map(value => Number(value) || 0) };
      }),
    };
    case 'task_list': return { type: 'checklist', items: list(raw.items).map(entry => {
      const item = obj(entry);
      return { text: text(item.label ?? item.text), done: item.state === 'done' || item.state === 'complete' };
    }).filter(item => item.text) };
    case 'progress': return { type: 'progress', label: text(raw.label ?? raw.title), value: Number(raw.value) || 0, max: Number(raw.max) || 100 };
    case 'code': return { type: 'code', file: text(raw.file), language: text(raw.language), code: text(raw.code).slice(0, 24000) };
    case 'diff': return { type: 'diff', file: text(raw.file), diff: text(raw.diff).slice(0, 24000) };
    case 'details': return { type: 'card', title: text(raw.summary), children: [view(raw.body, depth + 1, count)].filter(Boolean) };
    case 'layout': return { type: 'stack', children: list(raw.children).map(child => view(child, depth + 1, count)).filter(Boolean) };
    case 'list': return { type: 'markdown', text: list(raw.items).map(entry => {
      const item = obj(entry);
      return `- ${text(item.text ?? item.label)}`;
    }).join('\n') };
    case 'approval': return {
      type: 'actions', label: `${text(raw.title)}: ${text(raw.detail)}`,
      buttons: [raw.approve, raw.deny].map(entry => {
        const action = obj(entry);
        return { label: text(action.label), prompt: text(action.label), tone: 'secondary' };
      }).filter(button => button.label),
    };
    case 'form': return {
      type: 'form', title: text(raw.title),
      fields: list(raw.fields).map(entry => {
        const field = obj(entry);
        return { name: text(field.name).replace(/[^\w-]/g, ''), label: text(field.label), type: text(field.type) || 'text', options: list(field.options).map(option => text(option)), required: field.required === true };
      }).filter(field => field.name),
      submit: text(raw.submit) || 'Add to prompt',
    };
    case 'custom': return null;
    default: return { type: 'markdown', text: `«${type || 'component'}»` };
  }
}

/** Returns the JSON spec a `<pantheon-ui>` fence needs, or null for empty documents. */
export function uiDocumentSpec(value: unknown): string | null {
  const event = obj(value);
  const document = obj(event.document);
  if (document.version !== 1 || !text(document.id) || !document.root) return null;
  const mapped = view(document.root, 0, { value: 0 });
  if (!mapped) return null;
  const spec = JSON.stringify(mapped);
  return spec.length <= LIMITS.source ? spec : null;
}
