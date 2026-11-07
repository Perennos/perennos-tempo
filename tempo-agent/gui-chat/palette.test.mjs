// Rehearsal for the slash menu: ranking, blanks, cards, ghost text. Run: node --test tempo-agent/gui-chat/palette.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fuzzy, rank, grouped, buildItems, blanks, nextBlank, subst, toolTemplate, commandFill, commandRuns, slashQuery, ghostFor, pickCards, has, guideForError, shuffle } from './palette.mjs';

const P = JSON.parse(readFileSync(new URL('./prompts.json', import.meta.url)));
const cat = {
  commands: [
    { name: '/cd', args: '<folder>', help: 'work in another folder', kind: 'command' },
    { name: '/mode', args: '[name]', help: 'default, accept-edits, plan', kind: 'command' },
    { name: '/undo', args: '', help: 'put back the files', kind: 'command' },
    { name: '/soundcheck', args: '[what]', help: 'review the changes here', kind: 'command' },
    { name: '/status', args: '', help: 'folder, mode, model', kind: 'command' },
    { name: '!', args: '<command>', help: 'run a command yourself', kind: 'syntax' },
    { name: '@file', args: '', help: 'attach a file', kind: 'syntax' },
  ],
  tools: [
    { name: 'grep', description: 'Search inside files.', approval: false },
    { name: 'list_dir', description: 'See what is in a folder.', approval: false },
    { name: 'run_command', description: 'Run a command.', approval: true },
    { name: 'search_memory', description: 'Look through your notes.', approval: false },
    { name: 'fetch_url', description: 'Read a web page.', approval: true },
  ],
  mcp: [{ server: 'vault', tool: 'read_note', name: 'vault__read_note', description: 'Read a note.', approval: false }],
  custom: [{ name: '/fixit', description: 'Fix the failing test.' }],
  bands: [{ name: 'social-media', about: 'posts', flows: [{ name: 'weekly-plan', goal: 'Plan the week.', every: 'manual' }], members: [] }],
  comfy: false, fleet: false,
};

test('fuzzy: letters in order, prefixes and word starts win', () => {
  assert.equal(fuzzy('zz', 'grep'), null);
  assert.ok(fuzzy('gr', 'grep').score > fuzzy('gr', 'a_big_grep').score, 'a prefix beats a late match');
  assert.ok(fuzzy('wp', 'weekly-plan').score > fuzzy('wp', 'wrapper').score, 'word starts beat scattered letters');
  assert.ok(fuzzy('undo', '/undo').score > fuzzy('undo', 'redo-and-undo-things').score);
  assert.deepEqual(fuzzy('sc', 'soundcheck').idx, [0, 5]);
  assert.ok(fuzzy('grep', 'grep').score > fuzzy('grep', 'grep_all').score, 'exact beats longer');
});

test('rank: empty keeps order, queries sort best first and drop misses', () => {
  const items = buildItems(cat, P);
  assert.equal(rank(items, '').length, items.length);
  const r = rank(items, 'sound');
  assert.equal(r[0].it.label, '/soundcheck');
  assert.ok(r.every(x => x.score > -Infinity));
  assert.equal(rank(items, 'qqqqzz').length, 0);
  assert.equal(rank(items, 'grep')[0].it.label, 'grep');
  assert.equal(rank(items, 'weekly')[0].it.group, 'band');
  assert.ok(rank(items, 'notes').some(x => x.it.label === 'search_memory'), 'a word in the description finds it');
});

test('grouped: every kind of thing is listed, commands first when nothing is typed', () => {
  const g = grouped(buildItems(cat, P), '');
  assert.deepEqual(g.map(x => x.group), ['command', 'custom', 'tool', 'mcp', 'band', 'app', 'guide']);
  const all = g.flatMap(x => x.rows.map(r => r.it.label));
  for (const n of ['/cd', '/mode', '/undo', '/soundcheck', '/status', '/fixit', 'grep', 'run_command', 'vault__read_note', 'Open Vault', 'Open Room', 'Open Studio', '/guide']) assert.ok(all.includes(n), n);
  assert.equal(grouped(buildItems(cat, P), 'run_c')[0].group, 'tool');
});

test('the approval badge follows the catalog', () => {
  const items = buildItems(cat, P);
  assert.equal(items.find(i => i.label === 'run_command').approval, true);
  assert.equal(items.find(i => i.label === 'grep').approval, false);
});

test('blanks: found, stepped through with Tab and Shift+Tab, wrapped', () => {
  const t = 'Use grep to find {text} in {folder}.';
  assert.deepEqual(blanks(t).map(b => b.name), ['text', 'folder']);
  const a = nextBlank(t, 0);
  assert.equal(t.slice(a.start, a.end), '{text}');
  const b = nextBlank(t, a.end);
  assert.equal(t.slice(b.start, b.end), '{folder}');
  assert.equal(nextBlank(t, b.end).name, 'text', 'wraps round');
  assert.equal(nextBlank(t, b.start, -1).name, 'text');
  assert.equal(nextBlank(t, a.start, -1).name, 'folder', 'back from the first wraps to the last');
  assert.equal(nextBlank('no blanks here', 0), null);
});

test('subst fills what it knows and leaves the rest as blanks', () => {
  assert.equal(subst('Run the {flow} flow of the {band} band, in {folder}.', { band: 'folk', flow: 'weekly-plan' }), 'Run the weekly-plan flow of the folk band, in {folder}.');
  assert.equal(subst('keep {this}', {}), 'keep {this}');
});

test('tool templates: every builtin has a natural prompt with blanks, and MCP tools get one', () => {
  for (const n of ['list_dir', 'read_file', 'write_file', 'edit_file', 'apply_patch', 'grep', 'glob', 'run_command', 'command_output', 'command_stop', 'trash', 'fetch_url', 'search_memory', 'workflow_update', 'delegate', 'comfy_image']) {
    const t = toolTemplate(n);
    assert.ok(t.includes(n) && blanks(t).length >= 1, n);
  }
  assert.equal(toolTemplate('grep'), 'Use grep to find {text} in {folder}.');
  assert.equal(toolTemplate('vault__read_note'), 'Use vault__read_note to {what you want done}.');
});

test('commands: Enter runs one that needs nothing, fills one that does', () => {
  const by = n => cat.commands.find(c => c.name === n);
  assert.equal(commandRuns(by('/undo')), true);
  assert.equal(commandRuns(by('/mode')), true, 'an optional argument still runs');
  assert.equal(commandRuns(by('/cd')), false);
  assert.equal(commandFill(by('/cd')), '/cd {folder}');
  assert.equal(commandFill(by('/mode')), '/mode ');
  assert.equal(commandFill(by('!')), '!{command}');
  assert.equal(commandFill(by('@file')), '@{file}');
  assert.equal(commandRuns(by('!')), false);
});

test('slashQuery and ghost text: slash commands only, never prose', () => {
  assert.equal(slashQuery('/so'), 'so');
  assert.equal(slashQuery('/'), '');
  assert.equal(slashQuery('/cd foo'), null);
  assert.equal(slashQuery('hello /so'), null);
  const names = ['/cd', '/mode', '/soundcheck', '/status'];
  assert.equal(ghostFor('/so', names), 'undcheck');
  assert.equal(ghostFor('/st', names), 'atus');
  assert.equal(ghostFor('/soundcheck', names), '');
  assert.equal(ghostFor('/', names), '');
  assert.equal(ghostFor('so', names), '', 'prose gets nothing');
  assert.equal(ghostFor('please /so', names), '');
  assert.equal(ghostFor('/zzz', names), '');
});

test('cards: fit this computer, never a missing tool, and they rotate', () => {
  const ids = c => c.map(x => x.id);
  const base = pickCards(P, cat, { net: true }, 1, 6);
  assert.ok(base.length >= 4 && base.length <= 6);
  assert.ok(!ids(base).includes('comfy') && !ids(base).includes('machines'), 'no ComfyUI, no fleet');
  const lean = pickCards(P, { commands: [], tools: [], mcp: [], bands: [] }, {}, 1, 6);
  assert.ok(lean.every(x => (x.needs || []).every(t => t.startsWith('no:'))), 'with nothing, only cards that need nothing');
  const rich = { ...cat, comfy: true, fleet: true, tools: [...cat.tools, { name: 'comfy_image' }, { name: 'delegate' }] };
  const seen = new Set();
  for (let s = 0; s < 40; s++) pickCards(P, rich, { net: true }, s, 6).forEach(x => seen.add(x.id));
  assert.ok(seen.has('comfy') && seen.has('machines'), 'they show up when they fit');
  const sets = new Set([1, 2, 3, 4, 5, 6].map(s => ids(pickCards(P, rich, { net: true }, s, 5)).join()));
  assert.ok(sets.size > 1, 'a new seed gives a new set');
  const withBand = pickCards(P, cat, { net: true }, 0, 6).concat(pickCards(P, cat, { net: true }, 3, 6)).find(x => x.id === 'band');
  if (withBand) assert.match(withBand.prompt, /weekly-plan flow of the social-media band/);
  assert.ok(!ids(pickCards(P, cat, {}, 2, 6)).includes('page'), 'no internet, no web card');
  assert.equal(ids(shuffle([1, 2, 3, 4], 7)).length, 4);
});

test('has: tokens read the catalog', () => {
  assert.equal(has('tool:grep', cat, {}), true);
  assert.equal(has('tool:nope', cat, {}), false);
  assert.equal(has('command:/undo', cat, {}), true);
  assert.equal(has('bands', cat, {}), true);
  assert.equal(has('no:bands', cat, {}), false);
  assert.equal(has('mcp:vault', cat, {}), true);
  assert.equal(has('net', cat, { net: true }), true);
});

test('guides: eleven of them, three examples each, and errors find their guide', () => {
  assert.ok(P.guides.length >= 10);
  for (const g of P.guides) assert.equal(g.examples.length, 3, g.id);
  const id = s => guideForError(s, P);
  assert.equal(id('refused: the write folder does not include that'), 'approvals');
  assert.equal(id('denied or timed out'), 'approvals');
  assert.equal(id('this needs the agent\'s key'), 'approvals');
  assert.equal(id('blocked by the owner\'s hook: no'), 'hooks');
  assert.equal(id('host unreachable on 192.0.2.1'), 'fleet');
  assert.equal(id('something odd'), 'ask');
  for (const e of P.errorGuides) assert.ok(P.guides.some(g => g.id === e.guide), e.guide);
});

test('house voice: no banned words in anything people read', () => {
  const text = JSON.stringify(P).toLowerCase();
  for (const w of ['robust', 'leverage', 'seamless', 'comprehensive', 'utilize', 'empower', 'synergy', 'cutting-edge', 'best-in-class', 'simply', 'ledger', 'open source']) assert.ok(!text.includes(w), w);
  assert.ok(!text.includes('!"') && !/[a-z]!\s/.test(text), 'no exclamation marks');
});
