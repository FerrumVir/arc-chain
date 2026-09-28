// A small, safe Markdown renderer for model answers. It builds React elements
// only (never HTML strings), so nothing in an answer can inject markup or
// script. It covers what answers actually use: paragraphs, line breaks,
// headings, lists, quotes, fenced code, inline code, bold and italic. Links are
// shown as their text followed by the address, never as clickable links: an
// answer must not be able to send someone to an arbitrary site from the app.

import { Check, Copy } from "lucide-react";
import { Fragment, useState, type ReactNode } from "react";

type Block =
  | { kind: "p"; lines: string[] }
  | { kind: "h"; level: number; text: string }
  | { kind: "ul" | "ol"; items: string[] }
  | { kind: "quote"; lines: string[] }
  | { kind: "code"; lang: string; text: string };

const UL = /^\s*[-*•]\s+(.*)$/;
const OL = /^\s*\d{1,3}[.)]\s+(.*)$/;
const H = /^(#{1,6})\s+(.*)$/;
const Q = /^>\s?(.*)$/;

export function parseBlocks(source: string): Block[] {
  const lines = source.replace(/\r\n?/g, "\n").replace(/^\s*\n/, "").replace(/\s+$/, "").split("\n");
  const blocks: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    const fence = line.match(/^\s*```\s*([\w+-]*)\s*$/);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) { body.push(lines[i]); i++; }
      i++; // the closing fence, if there is one
      blocks.push({ kind: "code", lang: fence[1] || "", text: body.join("\n") });
      continue;
    }
    const h = line.match(H);
    if (h) { blocks.push({ kind: "h", level: h[1].length, text: h[2] }); i++; continue; }
    // gather the run of non-blank lines up to the next blank line or fence
    const run: string[] = [];
    while (i < lines.length && lines[i].trim() && !/^\s*```/.test(lines[i]) && !H.test(lines[i])) { run.push(lines[i]); i++; }
    if (run.every((l) => UL.test(l))) blocks.push({ kind: "ul", items: run.map((l) => l.match(UL)![1]) });
    else if (run.every((l) => OL.test(l))) blocks.push({ kind: "ol", items: run.map((l) => l.match(OL)![1]) });
    else if (run.every((l) => Q.test(l))) blocks.push({ kind: "quote", lines: run.map((l) => l.match(Q)![1]) });
    else blocks.push({ kind: "p", lines: run });
  }
  return blocks;
}

// Inline marks: `code`, **bold**, __bold__, *italic* and [text](address). Underscore italics are left alone:
// identifiers like snake_case are far commoner in answers (and lookbehind would break older WebKit).
const INLINE = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(__[^_\n]+__)|(\*[^*\s][^*\n]*\*)|(\[[^\]\n]+\]\([^)\s]+\))/g;

export function renderInline(text: string, key = ""): ReactNode[] {
  const out: ReactNode[] = [];
  let last = 0;
  let m: RegExpExecArray | null;
  // A fresh expression per call: bold and italic recurse, and must not move the outer scan.
  const re = new RegExp(INLINE.source, "g");
  while ((m = re.exec(text))) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0], k = `${key}${m.index}`;
    if (m[1]) out.push(<code key={k} className="md-code">{tok.slice(1, -1)}</code>);
    else if (m[2] || m[3]) out.push(<strong key={k}>{renderInline(tok.slice(2, -2), k)}</strong>);
    else if (m[4]) out.push(<em key={k}>{renderInline(tok.slice(1, -1), k)}</em>);
    else if (m[5]) {
      const parts = tok.match(/^\[([^\]]+)\]\(([^)]+)\)$/)!;
      out.push(<Fragment key={k}>{parts[1]} <span className="md-address">({parts[2]})</span></Fragment>);
    }
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

function CodeBlock({ text, lang }: { text: string; lang: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="md-pre-wrap">
      <pre className="md-pre" data-lang={lang || undefined}><code>{text}</code></pre>
      <button
        type="button"
        className="md-copy"
        aria-label={copied ? "Code copied" : "Copy code"}
        title={copied ? "Copied" : "Copy code"}
        onClick={() => {
          void navigator.clipboard.writeText(text).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 1400);
          });
        }}
      >
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </button>
    </div>
  );
}

export function Markdown({ text, className, testId }: { text: string; className?: string; testId?: string }) {
  const blocks = parseBlocks(text);
  return (
    <div className={className} data-testid={testId}>
      {blocks.map((b, i) => {
        const k = `b${i}`;
        switch (b.kind) {
          case "h":
            return b.level <= 2 ? <h3 key={k} className="md-h">{renderInline(b.text, k)}</h3> : <h4 key={k} className="md-h md-h-sm">{renderInline(b.text, k)}</h4>;
          case "ul":
            return <ul key={k}>{b.items.map((it, j) => <li key={j}>{renderInline(it, `${k}-${j}-`)}</li>)}</ul>;
          case "ol":
            return <ol key={k}>{b.items.map((it, j) => <li key={j}>{renderInline(it, `${k}-${j}-`)}</li>)}</ol>;
          case "quote":
            return <blockquote key={k}>{b.lines.map((l, j) => <Fragment key={j}>{j > 0 && <br />}{renderInline(l, `${k}-${j}-`)}</Fragment>)}</blockquote>;
          case "code":
            return <CodeBlock key={k} text={b.text} lang={b.lang} />;
          default:
            return <p key={k}>{b.lines.map((l, j) => <Fragment key={j}>{j > 0 && <br />}{renderInline(l, `${k}-${j}-`)}</Fragment>)}</p>;
        }
      })}
    </div>
  );
}
