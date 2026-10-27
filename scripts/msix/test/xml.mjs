// Tiny strict XML parser for tests: elements, attributes, text, comments and the
// XML declaration. Throws on mismatched/unclosed tags, duplicate attributes and
// stray "&" / "<", which is enough to prove the generated manifest is
// well-formed before makeappx (or Partner Center) sees it.

const ENTITIES = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'" };

function decode(s, where) {
  return s.replace(/&([^;]*);?/g, (m, name) => {
    if (!m.endsWith(";") || !(name in ENTITIES)) throw new Error(`bad entity "${m}" in ${where}`);
    return ENTITIES[name];
  });
}

export function parseXml(src) {
  let i = 0;
  const root = { name: "#document", attrs: {}, children: [] };
  const stack = [root];
  const top = () => stack[stack.length - 1];
  while (i < src.length) {
    if (src.startsWith("<?", i)) {
      const end = src.indexOf("?>", i);
      if (end < 0) throw new Error("unterminated declaration");
      i = end + 2;
    } else if (src.startsWith("<!--", i)) {
      const end = src.indexOf("-->", i);
      if (end < 0) throw new Error("unterminated comment");
      i = end + 3;
    } else if (src.startsWith("</", i)) {
      const end = src.indexOf(">", i);
      const name = src.slice(i + 2, end).trim();
      const open = stack.pop();
      if (!open || open.name !== name) throw new Error(`mismatched </${name}> (open: ${open && open.name})`);
      i = end + 1;
    } else if (src[i] === "<") {
      const m = /^<([A-Za-z_][\w:.-]*)((?:\s+[\w:.-]+\s*=\s*"[^"]*")*)\s*(\/?)>/.exec(src.slice(i));
      if (!m) throw new Error(`malformed tag at offset ${i}: ${src.slice(i, i + 40)}`);
      const el = { name: m[1], attrs: {}, children: [] };
      for (const a of m[2].matchAll(/([\w:.-]+)\s*=\s*"([^"]*)"/g)) {
        if (a[1] in el.attrs) throw new Error(`duplicate attribute ${a[1]} on <${el.name}>`);
        if (a[2].includes("<")) throw new Error(`"<" in attribute ${a[1]}`);
        el.attrs[a[1]] = decode(a[2], `@${a[1]}`);
      }
      top().children.push(el);
      if (!m[3]) stack.push(el);
      i += m[0].length;
    } else {
      const end = src.indexOf("<", i);
      const text = src.slice(i, end < 0 ? src.length : end);
      if (text.trim()) {
        if (stack.length === 1) throw new Error("text outside the root element");
        top().children.push({ text: decode(text, `<${top().name}>`) });
      }
      i = end < 0 ? src.length : end;
    }
  }
  if (stack.length !== 1) throw new Error(`unclosed <${top().name}>`);
  const elements = root.children.filter((c) => c.name);
  if (elements.length !== 1) throw new Error("document must have exactly one root element");
  return elements[0];
}

/** First descendant matching a "/"-separated element path, e.g. "Applications/Application". */
export function find(el, path) {
  let cur = el;
  for (const seg of path.split("/")) {
    cur = cur && cur.children.find((c) => c.name === seg);
  }
  return cur || null;
}

export function textOf(el) {
  return el ? el.children.filter((c) => "text" in c).map((c) => c.text).join("") : null;
}
