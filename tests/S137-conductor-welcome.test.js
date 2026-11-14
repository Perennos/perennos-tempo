'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '..', 'perennos-tempo-launcher', 'src', 'conductor', 'welcome.js'), 'utf8');
const saved = new Map();
function run(profileId = 'profile-1') {
  const events = new Map();
  let activeElement;
  const steps = [0, 1, 2].map(() => ({ hidden: false, querySelector: () => ({ focus() {} }) }));
  const node = () => ({ hidden: false, textContent: '', disabled: false, addEventListener(type, fn) { events.set(this.id + ':' + type, fn); }, focus() { activeElement = this; }, id: '' });
  const body = node(); activeElement = body;
  const ids = Object.fromEntries(['conductor-tour-progress', 'conductor-tour-back', 'conductor-tour-next', 'conductor-tour-dismiss', 'conductor-tour-title', 'company-title'].map(id => { const n = node(); n.id = id; n.scrolled = false; n.scrollIntoView = () => { n.scrolled = true; }; return [id, n]; }));
  const root = {
    open: false, showCount: 0, closeCount: 0,
    showModal() { this.open = true; this.showCount++; },
    close() { this.open = false; this.closeCount++; },
    setAttribute() {},
    querySelectorAll(selector) {
      if (selector === '[data-tour-step]') return steps;
      return [ids['conductor-tour-dismiss'], ids['conductor-tour-back'], ids['conductor-tour-next']].filter(button => !button.disabled);
    },
    addEventListener(type, fn) { events.set('root:' + type, fn); },
    focus() { activeElement = this; }
  };
  const document = { get activeElement() { return activeElement; }, getElementById(id) { return id === 'conductor-first-run' ? root : ids[id]; }, querySelector(selector) { return selector === '.tempo-tabs' ? { dataset: { profileId } } : {}; } };
  const window = {};
  const localStorage = { getItem(key) { return saved.get(key) ?? null; }, setItem(key, value) { saved.set(key, value); } };
  vm.runInNewContext(source, { document, localStorage, window });
  return { root, steps, ids, events, document, body, window };
}

const first = run();
assert.equal(first.root.open, false, 'the app waits until its first-run scan finishes');
first.window.startConductorTour();
assert.equal(first.root.open, true, 'first Conductor open shows the modal tour');
assert.equal(first.root.showCount, 1, 'the native modal opens once');
assert.equal(first.ids['conductor-tour-progress'].textContent, '1 of 3');
assert.equal(first.document.activeElement, first.ids['conductor-tour-title'], 'opening moves focus into the tour');
const tabFromTitle = { key: 'Tab', shiftKey: false, prevented: false, preventDefault() { this.prevented = true; } };
first.events.get('root:keydown')(tabFromTitle);
assert.equal(tabFromTitle.prevented, true, 'Tab stays inside the modal');
assert.equal(first.document.activeElement, first.ids['conductor-tour-dismiss']);
first.ids['conductor-tour-next'].focus();
const tabFromLast = { key: 'Tab', shiftKey: false, prevented: false, preventDefault() { this.prevented = true; } };
first.events.get('root:keydown')(tabFromLast);
assert.equal(first.document.activeElement, first.ids['conductor-tour-dismiss'], 'Tab wraps to the first control');
first.events.get('conductor-tour-next:click')();
assert.equal(first.steps[1].hidden, false, 'Continue advances to the operations map');
first.events.get('conductor-tour-next:click')();
assert.equal(first.steps[2].hidden, false, 'Continue advances to standings');
first.events.get('conductor-tour-next:click')();
assert.equal(first.root.open, false, 'Done closes the tour');
assert.equal(first.root.closeCount, 1, 'closing uses the native dialog close path');
assert.equal(first.document.activeElement, first.ids['company-title'], 'closing moves focus to the org chart editor');
assert.equal(first.ids['company-title'].scrolled, true, 'closing brings the editor into view');
const second = run();
second.window.startConductorTour();
assert.equal(second.root.open, false, 'saved dismissal keeps the tour closed on the next open');
const freshProfile = run('profile-2');
freshProfile.window.startConductorTour();
assert.equal(freshProfile.root.open, true, 'a different app profile gets its own first-open tour');
process.stdout.write('1 test passed: native Conductor tour is keyboard accessible, modal, one-time per profile, and focuses the org editor.\n');
