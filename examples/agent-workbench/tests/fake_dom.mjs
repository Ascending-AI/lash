// A small tree DOM for running the production timeline module in Node.
//
// It implements the subset of the DOM the timeline uses, as a real tree:
// nodes have one parent, insertion moves a node, and every child removal is
// recorded on `document.removals`, which is what the laws read where a
// browser would use a MutationObserver.

class FakeNode {
  constructor(document) {
    this.ownerDocument = document;
    this.parentNode = null;
    this.childNodes = [];
  }

  get parentElement() {
    return this.parentNode;
  }

  get firstChild() {
    return this.childNodes[0] || null;
  }

  get lastChild() {
    return this.childNodes.at(-1) || null;
  }

  get nextSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode.childNodes;
    return siblings[siblings.indexOf(this) + 1] || null;
  }

  get isConnected() {
    let node = this;
    while (node.parentNode) node = node.parentNode;
    return node === this.ownerDocument.body;
  }

  remove() {
    if (!this.parentNode) return;
    const parent = this.parentNode;
    parent.childNodes.splice(parent.childNodes.indexOf(this), 1);
    this.parentNode = null;
    this.ownerDocument.removals.push({ parent, node: this });
  }
}

class FakeText extends FakeNode {
  constructor(document, text) {
    super(document);
    this.nodeType = 3;
    this.nodeValue = String(text);
  }

  get textContent() {
    return this.nodeValue;
  }

  set textContent(value) {
    this.nodeValue = String(value);
  }
}

class FakeElement extends FakeNode {
  constructor(document, tagName) {
    super(document);
    this.nodeType = 1;
    this.tagName = tagName.toUpperCase();
    this.className = "";
    this.dataset = {};
    this.attributes = {};
    this.listeners = {};
    this.hidden = false;
    this.html = null;
  }

  get classList() {
    const element = this;
    const names = () => element.className.split(/\s+/).filter(Boolean);
    return {
      contains: name => names().includes(name),
      add: (...added) => { element.className = [...new Set([...names(), ...added])].join(" "); },
      remove: (...removed) => { element.className = names().filter(name => !removed.includes(name)).join(" "); },
      toggle(name, force) {
        const on = force === undefined ? !names().includes(name) : Boolean(force);
        if (on) this.add(name);
        else this.remove(name);
        return on;
      }
    };
  }

  get children() {
    return this.childNodes.filter(node => node.nodeType === 1);
  }

  get textContent() {
    return this.childNodes.map(node => node.textContent).join("");
  }

  set textContent(value) {
    this.replaceChildren(String(value));
  }

  get innerHTML() {
    return this.html ?? this.textContent;
  }

  /* Markup is kept as given; its text is what a reader would see. */
  set innerHTML(value) {
    this.replaceChildren(String(value).replace(/<[^>]*>/g, ""));
    this.html = String(value);
  }

  adopt(child) {
    const node = typeof child === "string" ? new FakeText(this.ownerDocument, child) : child;
    if (node.parentNode) node.remove();
    node.parentNode = this;
    return node;
  }

  appendChild(child) {
    const node = this.adopt(child);
    this.childNodes.push(node);
    this.html = null;
    return node;
  }

  append(...children) {
    for (const child of children) this.appendChild(child);
  }

  insertBefore(child, reference) {
    if (!reference) return this.appendChild(child);
    const node = this.adopt(child);
    this.childNodes.splice(this.childNodes.indexOf(reference), 0, node);
    this.html = null;
    return node;
  }

  replaceChildren(...children) {
    for (const node of [...this.childNodes]) node.remove();
    this.append(...children);
  }

  replaceWith(node) {
    const parent = this.parentNode;
    if (!parent) return;
    const next = this.nextSibling;
    this.remove();
    parent.insertBefore(node, next);
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
  }

  getAttribute(name) {
    return this.attributes[name] ?? null;
  }

  removeAttribute(name) {
    delete this.attributes[name];
  }

  addEventListener(type, listener) {
    (this.listeners[type] ||= []).push(listener);
  }

  dispatch(type, event = {}) {
    for (const listener of this.listeners[type] || []) listener({ preventDefault() {}, stopPropagation() {}, ...event });
  }

  /* Class-name selectors only (`.a.b`, optionally `tag.a`), which is all the
     laws and the renderer harness ask of it. */
  querySelectorAll(selector) {
    const [tag, ...classes] = selector.split(".");
    const found = [];
    const visit = node => {
      for (const child of node.children) {
        if ((!tag || child.tagName === tag.toUpperCase()) && classes.every(name => child.classList.contains(name))) found.push(child);
        visit(child);
      }
    };
    visit(this);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] || null;
  }
}

export function createFakeDocument() {
  const document = { removals: [] };
  document.createElement = tag => new FakeElement(document, tag);
  document.createTextNode = text => new FakeText(document, text);
  document.body = new FakeElement(document, "body");
  return document;
}
