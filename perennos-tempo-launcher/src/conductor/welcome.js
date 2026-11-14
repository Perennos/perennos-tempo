'use strict';
(() => {
  const root = document.getElementById('conductor-first-run');
  if (!root) return;
  const profileId = document.querySelector('.tempo-tabs')?.dataset.profileId || 'default';
  const key = `perennos-tempo.conductor-tour.v1.${profileId}`;
  const steps = [...root.querySelectorAll('[data-tour-step]')];
  const progress = document.getElementById('conductor-tour-progress');
  const back = document.getElementById('conductor-tour-back');
  const next = document.getElementById('conductor-tour-next');
  const dismiss = document.getElementById('conductor-tour-dismiss');
  const title = document.getElementById('conductor-tour-title');
  let current = 0, previousFocus = null, started = false;
  function show(index) {
    current = Math.max(0, Math.min(steps.length - 1, index));
    steps.forEach((step, i) => { step.hidden = i !== current; });
    progress.textContent = `${current + 1} of ${steps.length}`;
    root.setAttribute('aria-describedby', steps[current].id);
    back.disabled = current === 0;
    next.textContent = current === steps.length - 1 ? 'Done' : 'Continue';
    if (current > 0) steps[current].querySelector('h3')?.focus({ preventScroll: true });
  }
  function start() {
    if (started) return;
    started = true;
    try { if (localStorage.getItem(key) === 'dismissed') return; } catch (_) { /* Keep the first-run tour available for this page session. */ }
    previousFocus = document.activeElement;
    if (!root.open) root.showModal();
    show(0);
    title?.focus({ preventScroll: true });
  }
  function close() {
    if (root.open) root.close();
    try { localStorage.setItem(key, 'dismissed'); } catch (_) { /* The tour still closes for this page session. */ }
    const editorTitle = document.getElementById('company-title');
    if (editorTitle) {
      editorTitle.tabIndex = -1;
      editorTitle.focus();
      editorTitle.scrollIntoView?.({ block: 'start' });
    } else previousFocus?.focus?.();
  }
  root.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); close(); return; }
    if (event.key !== 'Tab') return;
    const buttons = [...root.querySelectorAll('button:not(:disabled)')];
    const first = buttons[0], last = buttons[buttons.length - 1];
    if (!first || !last) return;
    if (event.shiftKey && [first, title, root].includes(document.activeElement)) {
      event.preventDefault(); last.focus();
    } else if (!event.shiftKey && [last, title, root].includes(document.activeElement)) {
      event.preventDefault(); first.focus();
    }
  });
  next.addEventListener('click', () => current === steps.length - 1 ? close() : show(current + 1));
  back.addEventListener('click', () => show(current - 1));
  dismiss.addEventListener('click', close);
  window.startConductorTour = start;
  if (!document.querySelector('script[src="/workbench.js"]')) start();
})();
