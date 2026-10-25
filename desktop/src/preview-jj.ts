// Browser-preview mock of the jj_* commands (native side: src-tauri/src/jj.rs).
import type { JjChange, JjStatus } from './jj';

const change = (changeId: string, description: string, extra: Partial<JjChange> = {}): JjChange =>
  ({ changeId, commitId: changeId.split('').reverse().join(''), description, bookmarks: [], author: 'Preview User', when: 'just now', conflict: false, empty: false, current: false, ...extra });

let counter = 0;
let current = change('kntqzsqtwlyn', 'Speed up search', { current: true });
let history: JjChange[] = [change('vruxwmqvtpmx', 'Add search index', { bookmarks: ['main'], when: '2 hours ago' }), change('zzzzzzzzzzzz', '', { empty: true, author: '', when: '' })];
let files = [{ status: 'M', path: 'src/lib/search-index.ts' }, { status: 'A', path: 'src/components/search.ts' }];

const status = (): JjStatus => ({ isRepo: true, colocated: mode() === 'colocated', change: current, files });
/** Off unless a test sets `window.__previewJj` to `true` or `'colocated'`, so other suites keep Git. */
const mode = () => (globalThis as any).__previewJj as boolean | 'colocated' | undefined;
const fresh = () => change(`preview${String(++counter).padStart(5, '0')}`, '', { current: true, empty: true });
/** Replaces the working-copy change with a new empty one on top of it. */
const advance = (description: string) => {
  history = [{ ...current, description, current: false }, ...history];
  current = fresh(); files = [];
};

export function invokeJj<T>(command: string, args: Record<string, any>): T {
  switch (command) {
    case 'jj_status': return (mode() ? status() : { isRepo: false, colocated: false, change: null, files: [] }) as T;
    case 'jj_log': return [current, ...history].slice(0, args.limit ?? 20) as T;
    case 'jj_diff': return `diff --git a/${args.path} b/${args.path}\n--- a/${args.path}\n+++ b/${args.path}\n@@ -1 +1 @@\n-old\n+new\n` as T;
    case 'jj_describe': current = { ...current, description: String(args.message ?? '') }; return status() as T;
    case 'jj_new': advance(current.description); return status() as T;
    case 'jj_commit':
      if (!String(args.message ?? '').trim()) throw new Error('Write a description before committing.');
      advance(String(args.message)); return status() as T;
    default: throw new Error(`Unknown jj command ${command}`);
  }
}
