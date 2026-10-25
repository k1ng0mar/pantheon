/* Browser-preview stand-in for the recipe commands: two saved recipes, an in-memory store, and
 * a small copy of the native dry run (statuses only; the Rust tests are the source of truth). */
import type { Recipe, RecipePlan, RecipeStep } from './recipes';

let store: Recipe[] = [
  { name: 'review-diff', description: 'Review, check, then stop for approval', scope: 'project', steps: [
    { kind: 'prompt', text: 'Review the changed files and summarize risks.' },
    { kind: 'slash', command: '/review', text: 'Review the diff for bugs.' },
    { kind: 'check', text: 'npm test' },
    { kind: 'approval', text: 'Ready to commit?' }] },
  { name: 'needs-ship', description: 'Blocked without /ship', scope: 'global', steps: [{ kind: 'slash', command: '/ship' }] },
];

function status(step: RecipeStep, slash: string[]): { status: string; reason: string } {
  if (step.kind === 'approval') return { status: 'manual', reason: 'Waits for you to confirm before the next step.' };
  if (step.kind !== 'slash') return { status: 'supported', reason: 'Sent as a message.' };
  const name = (step.command ?? '').split(/\s+/)[0];
  if (slash.includes(name)) return { status: 'supported', reason: `${name} is offered.` };
  return step.text ? { status: 'fallback', reason: `${name} is not offered; the step's own text is sent instead.` } : { status: 'blocked', reason: `${name} is not offered and the step has no fallback text.` };
}

export function recipesCommand(command: string, args: Record<string, any>): { handled: boolean; result?: unknown } {
  switch (command) {
    case 'recipes_list': return { handled: true, result: structuredClone(store) };
    case 'recipes_save': {
      const recipe = args.recipe as Recipe;
      if (!/^[a-z0-9][a-z0-9_-]{0,63}$/.test(recipe.name) || !recipe.steps?.length) throw 'Recipe names use lowercase letters, digits, - and _ (up to 64).';
      store = [...store.filter(r => r.name !== recipe.name), { ...recipe, scope: args.scope }];
      return { handled: true, result: null };
    }
    case 'recipes_delete': store = store.filter(r => r.name !== args.name); return { handled: true, result: null };
    case 'recipes_dry_run': {
      const recipe = args.recipe as Recipe;
      const steps = recipe.steps.map((s, index) => ({ index, ...status(s, args.slash ?? []) }));
      const blocked = steps.some(s => s.status === 'blocked');
      const messages = blocked ? [] : recipe.steps.map(s => (s.kind === 'slash' ? s.command! : s.kind === 'check' ? `Run this check and report the result, including any failures:\n${s.text}` : s.kind === 'approval' ? 'Stop here and ask me to confirm before doing anything else. Do not continue until I reply.' : s.text!));
      return { handled: true, result: { steps, messages } as RecipePlan };
    }
    default: return { handled: false };
  }
}
