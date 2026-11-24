/**
 * Finds the messages that the engine's side of the app writes, for the tests of
 * their translations. Nothing of this is part of the app.
 */

import { parseSync } from "vite";

/** Calls whose arguments are compared, logged or sent to the other end of a connection. */
const INTERNAL_CALLS = new Set(["close", "endsWith", "includes", "startsWith", "test"]);

interface Node {
  readonly type: string;
  readonly [key: string]: unknown;
}

function isNode(value: unknown): value is Node {
  return (
    typeof value === "object" && value !== null && "type" in value && typeof value.type === "string"
  );
}

function nodes(value: unknown): Node[] {
  return (Array.isArray(value) ? value : [value]).filter(isNode);
}

/** The name that something goes by in a message. */
function nameOf(value: unknown): string {
  const [node] = nodes(value);
  if (node?.type === "Identifier") {
    return String(node.name);
  }
  return node?.type === "MemberExpression" ? nameOf(node.property) : "value";
}

/** What a literal says, with the names of what is inserted in placeholders. */
function textOf(node: Node): string | null {
  if (node.type === "Literal") {
    return typeof node.value === "string" ? node.value : null;
  }
  if (node.type !== "TemplateLiteral") {
    return null;
  }
  const inserted = nodes(node.expressions);
  return nodes(node.quasis)
    .map(({ value }, index) => {
      const cooked =
        typeof value === "object" && value !== null && "cooked" in value ? value.cooked : "";
      const next = inserted[index];
      return String(cooked) + (next ? `{${nameOf(next)}}` : "");
    })
    .join("");
}

/** Whether a text has several words, or is a single one such as a label has. */
export function isEnglish(text: string): boolean {
  if (text.trim().split(/\s+/).length === 1) {
    return /^[A-Z][a-z-]{3,}$/.test(text);
  }
  const words = text.replace(/\{\w+\}/g, "").split(/\s+/);
  return words.some((word) => /[a-z].*[a-z]/i.test(word) && !/[_=]/.test(word));
}

function visit(node: Node, found: string[]): void {
  if (node.type === "ImportDeclaration" || node.type === "TSLiteralType") {
    return;
  }
  if (node.type === "CallExpression") {
    const [callee] = nodes(node.callee);
    const isLog = callee?.type === "MemberExpression" && nameOf(callee.object) === "console";
    if (isLog || INTERNAL_CALLS.has(nameOf(callee))) {
      return;
    }
  }
  const text = textOf(node);
  if (text !== null && isEnglish(text)) {
    found.push(text);
  }
  for (const value of Object.values(node)) {
    for (const child of nodes(value)) {
      visit(child, found);
    }
  }
}

/** Whatever reads like English in a module. */
export function messagesIn(path: string, source: string): string[] {
  const { program, errors } = parseSync(path, source);
  if (errors.length > 0) {
    throw new Error(`cannot parse ${path}: ${errors[0]?.message}`);
  }
  const found: string[] = [];
  for (const root of nodes(program)) {
    visit(root, found);
  }
  return found;
}
