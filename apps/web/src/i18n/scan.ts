/**
 * Finds the texts of the interface in the sources, for `completeness.test.ts`.
 * Nothing of this is part of the app.
 */

import { posix } from "node:path";
import { parseSync } from "vite";

export interface TextCall {
  readonly kind: "t" | "tn" | "tx";
  /** The key, and for `tn` the English singular before it. */
  readonly key: string;
  readonly singular: string | null;
  /** The names a params literal gives, or `null` if there is none to read. */
  readonly params: readonly string[] | null;
  /** The slots an object literal gives to `tx`, or `null` if there is none to read. */
  readonly slots: readonly string[] | null;
  readonly line: number;
}

export interface Finding {
  readonly line: number;
  readonly text: string;
}

export interface ScannedModule {
  /** The modules it refers to, as it spells them, without types. */
  readonly staticImports: readonly string[];
  readonly dynamicImports: readonly string[];
  readonly calls: readonly TextCall[];
  /** Uses of `t`, `tn` and `tx` that the catalogues cannot be checked against. */
  readonly misuses: readonly Finding[];
  /**
   * English that reaches the user without any of them: what is rendered, what
   * is given to a function that shows it, and whatever else reads like English.
   */
  readonly bareTexts: readonly Finding[];
}

/** The functions whose calls are texts, by the names they are exported under. */
const TEXT_FUNCTIONS = new Set(["t", "tn", "tx"]);

/** The attributes that put their value on the screen or have it read out. */
const TEXT_ATTRIBUTES = new Set([
  "title",
  "aria-label",
  "aria-description",
  "aria-roledescription",
  "aria-valuetext",
  "placeholder",
  "alt",
  "label",
]);

/** The options and fields that end up as a name on the screen, in modules with JSX. */
const TEXT_PROPERTIES = new Set(["label", "ariaLabel", "placeholder", "tooltip"]);

/** The properties of elements that show the text they are set to. */
const TEXT_MEMBERS = new Set([
  "textContent",
  "innerText",
  "title",
  "placeholder",
  "ariaLabel",
  "alt",
]);

/** The functions that show the text they are given: setters of states that are rendered, and callbacks. */
const NOTIFIERS =
  /^(?:(?:set|on)\w*(?:Notice|Error|Message|Status|Banner|Reason|Rejected)|announce|notify|alert|confirm|prompt)$/;

/** The functions whose arguments are stored, sent to others or name a file. */
const KEEPERS =
  /^(?:createWorkbook|renameWorkbook|downloadArrayBuffer|triggerBrowserDownload|send|sendHost|sendSync|on(?:Create|Rename|Import)[A-Z]\w*|handleCreate\w*|\w*File[Nn]ame)$/;

const LETTER = /\p{L}/u;

/** The beginning of a title or a sentence. */
const PROSE = /^[A-Z][a-z]+[\w’',.…-]* \S/;

interface Node {
  readonly type: string;
  readonly start: number;
  readonly [key: string]: unknown;
}

function isNode(value: unknown): value is Node {
  return (
    typeof value === "object" &&
    value !== null &&
    "type" in value &&
    typeof value.type === "string" &&
    "start" in value
  );
}

function node(value: unknown): Node | null {
  return isNode(value) ? value : null;
}

function nodes(value: unknown): Node[] {
  return Array.isArray(value) ? value.filter(isNode) : [];
}

function nameOf(value: unknown): string | null {
  const found = node(value);
  return (found?.type === "Identifier" || found?.type === "JSXIdentifier") &&
    typeof found.name === "string"
    ? found.name
    : null;
}

/** The value of a string literal, or of a template without anything inserted. */
function literalText(value: unknown): string | null {
  const found = node(value);
  if (found?.type === "Literal" && typeof found.value === "string") {
    return found.value;
  }
  if (found?.type === "TemplateLiteral" && nodes(found.expressions).length === 0) {
    return templateTexts(found).join("");
  }
  return null;
}

function templateTexts(template: Node): string[] {
  return nodes(template.quasis).map((quasi) => {
    const { value } = quasi;
    return typeof value === "object" && value !== null && "cooked" in value
      ? String(value.cooked)
      : "";
  });
}

/** The name a function is called by: `f` of `f()` and of `a.b?.f()`. */
function calleeName(call: Node): string | null {
  const callee = node(call.callee);
  return callee?.type === "MemberExpression" ? nameOf(callee.property) : nameOf(callee);
}

/** The names of an object literal's properties, or `null` if it is not one or has others. */
function propertyNames(value: unknown): string[] | null {
  const object = node(value);
  if (object?.type !== "ObjectExpression") {
    return null;
  }
  const names: string[] = [];
  for (const property of nodes(object.properties)) {
    const key = node(property.key);
    const name = nameOf(key) ?? (key?.type === "Literal" ? String(key.value) : null);
    if (property.type !== "Property" || property.computed === true || name === null) {
      return null;
    }
    names.push(name);
  }
  return names;
}

/** The parts of an expression that may become its value, as far as they are text. */
function* possibleTexts(value: unknown): Generator<{ at: Node; text: string }> {
  const found = node(value);
  switch (found?.type) {
    case "Literal":
      if (typeof found.value === "string") {
        yield { at: found, text: found.value };
      }
      break;
    case "TemplateLiteral":
      yield { at: found, text: templateTexts(found).join("{}") };
      break;
    case "ConditionalExpression":
      yield* possibleTexts(found.consequent);
      yield* possibleTexts(found.alternate);
      break;
    case "LogicalExpression":
    case "BinaryExpression":
      if (found.type === "LogicalExpression" || found.operator === "+") {
        yield* possibleTexts(found.left);
        yield* possibleTexts(found.right);
      }
      break;
    case "JSXExpressionContainer":
    case "ParenthesizedExpression":
    case "TSAsExpression":
    case "TSSatisfiesExpression":
    case "TSNonNullExpression":
      yield* possibleTexts(found.expression);
      break;
  }
}

/** Whether a module means one with `t`, `tn` or `tx` by a specifier. */
function isI18nModule(path: string, specifier: string): boolean {
  const meant = posix.join(posix.dirname(path), specifier);
  return /(?:^|\/)i18n\/(?:index|react)\.ts$/.test(meant);
}

class Scanner {
  readonly staticImports: string[] = [];
  readonly dynamicImports: string[] = [];
  readonly calls: TextCall[] = [];
  readonly misuses: Finding[] = [];
  readonly bareTexts: Finding[] = [];
  readonly #reported = new Set<number>();
  /** The local names of `t`, `tn` and `tx`. */
  readonly #functions = new Map<string, TextCall["kind"]>();
  readonly #path: string;
  readonly #hasJsx: boolean;
  readonly #lineStarts: number[];

  constructor(path: string, source: string) {
    this.#path = path;
    this.#hasJsx = path.endsWith(".tsx");
    this.#lineStarts = [0];
    for (const { index } of source.matchAll(/\n/g)) {
      this.#lineStarts.push(index + 1);
    }
  }

  #line(at: Node): number {
    return this.#lineStarts.findLastIndex((start) => start <= at.start) + 1;
  }

  #misuse(at: Node, text: string): void {
    this.misuses.push({ line: this.#line(at), text });
  }

  #bareText(at: Node, text: string): void {
    const flat = text.replace(/\s+/g, " ").trim();
    if (LETTER.test(flat) && !this.#reported.has(at.start)) {
      this.#reported.add(at.start);
      this.bareTexts.push({ line: this.#line(at), text: flat });
    }
  }

  #bareTexts(value: unknown): void {
    for (const { at, text } of possibleTexts(value)) {
      this.#bareText(at, text);
    }
  }

  /** The imports come first, wherever they are. */
  imports(program: Node): void {
    for (const statement of nodes(program.body)) {
      const specifier = literalText(statement.source);
      const isType = statement.importKind === "type" || statement.exportKind === "type";
      if (specifier === null || isType) {
        continue;
      }
      this.staticImports.push(specifier);
      if (!isI18nModule(this.#path, specifier)) {
        continue;
      }
      if (statement.type !== "ImportDeclaration") {
        this.#misuse(statement, "import the functions where they are used");
        continue;
      }
      for (const imported of nodes(statement.specifiers)) {
        const name = nameOf(imported.imported);
        const local = nameOf(imported.local);
        if (imported.type !== "ImportSpecifier") {
          this.#misuse(imported, "import the functions by their names");
        } else if (name !== null && local !== null && TEXT_FUNCTIONS.has(name)) {
          this.#functions.set(local, name as TextCall["kind"]);
        }
      }
    }
  }

  visit(at: Node, inFunction: boolean): void {
    let inside = inFunction;
    switch (at.type) {
      case "ImportDeclaration":
        return;
      case "FunctionDeclaration":
      case "FunctionExpression":
      case "ArrowFunctionExpression":
        inside = true;
        break;
      case "PropertyDefinition":
        inside ||= at.static !== true;
        break;
      case "ImportExpression": {
        const specifier = literalText(at.source);
        if (specifier !== null) {
          this.dynamicImports.push(specifier);
          if (isI18nModule(this.#path, specifier)) {
            this.#misuse(at, "import the functions by their names");
          }
        }
        break;
      }
      case "Literal":
      case "TemplateLiteral":
        for (const { text } of possibleTexts(at)) {
          if (PROSE.test(text)) {
            this.#bareText(at, text);
          }
        }
        break;
      case "NewExpression":
        // Like what goes to the console, it is for whoever reads the log.
        if (/Error$/.test(nameOf(at.callee) ?? "")) {
          return;
        }
        break;
      case "Identifier":
        if (this.#functions.has(nameOf(at) ?? "")) {
          this.#misuse(at, `${nameOf(at)} is not called here, or means something else`);
        }
        break;
      case "CallExpression":
        if (nameOf(node(at.callee)?.object) === "console") {
          return;
        }
        this.#call(at, inside);
        break;
      case "AssignmentExpression": {
        const target = node(at.left);
        if (target?.type === "MemberExpression" && nameOf(target.property) === "download") {
          this.#keeps(at, at.right);
        }
        if (
          target?.type === "MemberExpression" &&
          TEXT_MEMBERS.has(nameOf(target.property) ?? "")
        ) {
          this.#bareTexts(at.right);
        }
        break;
      }
      case "JSXText":
        this.#bareText(at, String(at.value));
        break;
      case "JSXElement":
      case "JSXFragment":
        for (const child of nodes(at.children)) {
          if (child.type === "JSXExpressionContainer") {
            this.#bareTexts(child);
          }
        }
        break;
      case "JSXAttribute":
        if (TEXT_ATTRIBUTES.has(nameOf(at.name) ?? "")) {
          this.#bareTexts(at.value);
        }
        break;
      case "Property":
        if (
          (this.#hasJsx && at.computed !== true && TEXT_PROPERTIES.has(nameOf(at.key) ?? "")) ||
          TEXT_ATTRIBUTES.has(literalText(at.key) ?? "")
        ) {
          this.#bareTexts(at.value);
        }
        break;
    }
    for (const [key, value] of Object.entries(at)) {
      // Not a use of the variable of that name.
      const isName =
        at.computed !== true &&
        (key === "key" || (at.type === "MemberExpression" && key === "property"));
      const isCallee =
        at.type === "CallExpression" && key === "callee" && this.#kindOf(at) !== null;
      const isType = at.type.startsWith("TS") && key === "typeName";
      if (isName || isCallee || isType) {
        continue;
      }
      // The texts themselves have been read.
      const isText = key === "arguments" && this.#kindOf(at) !== null;
      for (const child of Array.isArray(value) ? nodes(value) : nodes([value])) {
        if (!isText || literalText(child) === null) {
          this.visit(child, inside);
        }
      }
    }
  }

  #kindOf(call: Node): TextCall["kind"] | null {
    const name = call.type === "CallExpression" ? nameOf(call.callee) : null;
    return name === null ? null : (this.#functions.get(name) ?? null);
  }

  #call(at: Node, inFunction: boolean): void {
    const args = nodes(at.arguments);
    const name = calleeName(at);
    if (name === "setAttribute" && TEXT_ATTRIBUTES.has(literalText(args[0]) ?? "")) {
      this.#bareTexts(args[1]);
    } else if (name !== null && NOTIFIERS.test(name)) {
      for (const arg of args) {
        this.#bareTexts(arg);
      }
    } else if (name !== null && KEEPERS.test(name)) {
      for (const arg of args) {
        this.#keeps(at, arg);
      }
    }
    const kind = this.#kindOf(at);
    if (kind === null) {
      return;
    }
    if (!inFunction) {
      this.#misuse(at, `${kind}() outside a function reads one language for good`);
    }
    const first = kind === "tn" ? 1 : 0;
    const singular = kind === "tn" ? literalText(args[1]) : null;
    const key = literalText(args[kind === "tn" ? 2 : 0]);
    if (key === null || (kind === "tn" && singular === null)) {
      this.#misuse(args[first] ?? at, `${kind}() needs its text as a literal`);
      return;
    }
    const params = args[kind === "tn" ? 3 : kind === "tx" ? 2 : 1];
    this.calls.push({
      kind,
      key,
      singular,
      params: params ? propertyNames(params) : [],
      slots: kind === "tx" ? propertyNames(args[1]) : [],
      line: this.#line(at),
    });
  }

  /** Reports a translated text in something that is kept. */
  #keeps(at: Node, value: unknown): void {
    const found = node(value);
    if (!found) {
      return;
    }
    if (found.type === "CallExpression" && this.#kindOf(found) !== null) {
      this.#misuse(at, "a translated text is stored, sent or used as a file's name");
      return;
    }
    // What a callback does is its own affair.
    if (!/Function/.test(found.type)) {
      for (const child of Object.values(found)) {
        for (const each of Array.isArray(child) ? nodes(child) : nodes([child])) {
          this.#keeps(at, each);
        }
      }
    }
  }
}

export function scanModule(path: string, source: string): ScannedModule {
  const { program, errors } = parseSync(path, source);
  if (errors.length > 0) {
    throw new Error(`cannot parse ${path}: ${errors[0]?.message}`);
  }
  const root: unknown = program;
  if (!isNode(root)) {
    throw new Error(`${path} is not a program`);
  }
  const scanner = new Scanner(path, source);
  scanner.imports(root);
  scanner.visit(root, false);
  return scanner;
}

/** The `{names}` in a text, each once, in order. */
export function placeholders(text: string): string[] {
  return [...new Set([...text.matchAll(/\{(\w+)\}/g)].map((match) => match[1] ?? ""))].sort();
}

/** The slots in a text, as `<0>` or `<0/>`, in order. Anything that looks like a broken one is listed too. */
export function slots(text: string): string[] {
  return [...text.matchAll(/<\/?\d+\/?>/g)].map(([tag]) => tag).sort();
}
