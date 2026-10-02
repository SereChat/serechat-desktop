// Outlines the page for the agent (see browser.rs): its visible text, with
// links, buttons and fields numbered so browser_click and browser_type can
// name them. Evaluated as an expression; returns the outline as a string.
// The numbered elements stay in window.__serechatRefs until the next
// snapshot, and vanish when the page navigates.
//
// ponytail: frames are skipped; walk same-origin iframes if pages need them.
(() => {
  const MAX = 24000;
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME', 'OBJECT', 'EMBED', 'HEAD']);
  const BLOCK = new Set([
    'ADDRESS', 'ARTICLE', 'ASIDE', 'BLOCKQUOTE', 'BR', 'DD', 'DETAILS', 'DIALOG', 'DIV', 'DL', 'DT', 'FIELDSET', 'FIGCAPTION',
    'FIGURE', 'FOOTER', 'FORM', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'HEADER', 'HR', 'LI', 'MAIN', 'NAV', 'OL', 'P', 'PRE',
    'SECTION', 'TABLE', 'TD', 'TH', 'TR', 'UL',
  ]);
  const ROLES = new Set(['button', 'link', 'checkbox', 'radio', 'switch', 'tab', 'menuitem', 'option', 'textbox', 'searchbox', 'combobox', 'slider']);
  const clean = (text, max) => (text || '').replace(/\s+/g, ' ').trim().slice(0, max);

  const refs = [];
  const lines = [];
  let line = '';
  let size = 0;
  let truncated = false;
  const flush = () => {
    const text = clean(line, 2000);
    line = '';
    if (!text) return;
    if (size + text.length > MAX) {
      truncated = true;
      return;
    }
    lines.push(text);
    size += text.length + 1;
  };

  const interactive = (el) => {
    const tag = el.tagName;
    if (tag === 'A') return el.hasAttribute('href');
    if (tag === 'INPUT') return el.type !== 'hidden';
    if (tag === 'BUTTON' || tag === 'SELECT' || tag === 'TEXTAREA' || tag === 'SUMMARY') return true;
    if (el.isContentEditable) return !el.parentElement || !el.parentElement.isContentEditable;
    return ROLES.has(el.getAttribute('role'));
  };

  const kind = (el) => {
    const tag = el.tagName;
    if (tag === 'A') return 'link';
    if (tag === 'BUTTON' || tag === 'SUMMARY') return 'button';
    if (tag === 'SELECT') return 'dropdown';
    if (tag === 'TEXTAREA' || el.isContentEditable) return 'textbox';
    if (tag === 'INPUT') {
      const type = el.type;
      if (type === 'checkbox' || type === 'radio') return type;
      if (['submit', 'button', 'reset', 'image'].includes(type)) return 'button';
      return type === 'text' ? 'textbox' : `textbox (${type})`;
    }
    return el.getAttribute('role');
  };

  const label = (el) => {
    const labelled = el.labels && el.labels[0] ? el.labels[0].innerText : '';
    const image = el.querySelector && el.querySelector('img[alt]');
    // A dropdown's text is all its options; they are listed separately.
    const text = el.tagName === 'SELECT' ? '' : el.innerText;
    return clean(
      el.getAttribute('aria-label') || labelled || text || (el.type === 'submit' || el.type === 'button' ? el.value : '') ||
        el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('alt') || (image ? image.alt : ''),
      100,
    );
  };

  const describe = (el, ref) => {
    let text = `[${ref}] ${kind(el)}`;
    const name = label(el);
    if (name) text += ` "${name}"`;
    const tag = el.tagName;
    if (tag === 'A') text += ` → ${clean(el.href, 120)}`;
    if (tag === 'SELECT') {
      const chosen = el.selectedOptions[0];
      const options = Array.from(el.options).slice(0, 20).map((o) => clean(o.text, 40));
      text += ` = "${chosen ? clean(chosen.text, 60) : ''}" (options: ${options.join(' | ')}${el.options.length > 20 ? ' | …' : ''})`;
    } else if (el.type === 'checkbox' || el.type === 'radio') {
      if (el.checked) text += ' (checked)';
    } else if (tag === 'INPUT' || tag === 'TEXTAREA') {
      // Never show a password to the model.
      if (el.value) text += el.type === 'password' ? ' (filled)' : ` = "${clean(el.value, 200)}"`;
    }
    if (el.disabled) text += ' (disabled)';
    return text;
  };

  const shown = (el) => {
    if (getComputedStyle(el).display === 'contents') return true;
    return el.checkVisibility ? el.checkVisibility({ visibilityProperty: true }) : el.getClientRects().length > 0;
  };

  const walk = (node, depth) => {
    if (truncated || depth > 200) return;
    if (node.nodeType === Node.TEXT_NODE) {
      line += node.nodeValue + ' ';
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
    const el = node;
    if (el.nodeType === Node.ELEMENT_NODE) {
      if (SKIP.has(el.tagName) || !shown(el)) return;
      if (interactive(el)) {
        flush();
        refs.push(el);
        line = describe(el, refs.length);
        flush();
        return;
      }
    }
    const block = BLOCK.has(el.tagName);
    const heading = /^H[1-6]$/.test(el.tagName || '');
    if (block) flush();
    if (heading) line = '#'.repeat(Number(el.tagName[1])) + ' ';
    const children = el.tagName === 'SLOT' && el.assignedNodes().length ? el.assignedNodes() : (el.shadowRoot || el).childNodes;
    for (const child of children) walk(child, depth + 1);
    if (block) flush();
  };

  walk(document.body || document.documentElement, 0);
  flush();
  window.__serechatRefs = refs;
  const head = `Page: ${clean(document.title, 200)}\nURL: ${location.href}\n`;
  const tail = truncated ? '\n… (the page goes on; this outline stops here)' : '';
  return `${head}\n${lines.join('\n') || '(no visible text)'}${tail}`;
})()
