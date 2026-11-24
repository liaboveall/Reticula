import { inEnglish } from "../i18n/index.ts";
import { COMMANDS, type Command, GROUPS } from "./registry.ts";

/** What a command is found by: its name and group, in the language shown and in English. */
function textOf({ title, group, words = "" }: Command): string {
  const named = () => `${title()} ${GROUPS[group]()}`;
  return `${named()} ${inEnglish(named)} ${words}`.toLocaleLowerCase();
}

/**
 * The commands that all of the words that have been typed are part of. The ones
 * whose name begins with them come first, then those they are in as they are.
 */
export function findCommands(query: string): readonly Command[] {
  const wanted = query.toLocaleLowerCase().split(/\s+/).filter(Boolean);
  const phrase = wanted.join(" ");
  const rank = ({ title }: Command) => {
    const at = [title(), inEnglish(title)].map((name) => name.toLocaleLowerCase().indexOf(phrase));
    return at.includes(0) ? 0 : at.some((position) => position > 0) ? 1 : 2;
  };
  return COMMANDS.filter((command) => {
    const text = textOf(command);
    return wanted.every((word) => text.includes(word));
  }).sort((one, other) => rank(one) - rank(other));
}
