import type {
  TableCondition,
  TableNumberOp,
  TableTimeOp,
  TableValues,
} from "@reticula/protocol/table";
import {
  type FormEvent,
  type ReactNode,
  type RefObject,
  useEffect,
  useId,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { DialogHeader } from "../../commands/Dialogs.tsx";
import { fmtInt, t, tn } from "../../i18n/index.ts";
import { announce } from "../../ui/announcer.ts";
import { Dialog } from "../../ui/Dialog.tsx";
import type { TableFailure } from "../failure.ts";
import type { TableModel } from "./model.ts";
import { conditionOn, filteredBy, isNumeric, isTime } from "./query.ts";
import { failureOf } from "./session.ts";
import { failureText } from "./text.ts";

/** Values that are listed at a time. */
const MOST_VALUES = 1_000;
/** Typing asks for values once it has paused for this long. */
const PAUSE_MS = 200;

export interface FilterDialogProps {
  readonly model: TableModel;
  readonly kind: "values" | "condition";
  readonly col: number;
  readonly opener: RefObject<Element | null>;
  readonly onClose: () => void;
}

/** What the rows that are shown have in a column. */
export function FilterDialog({ model, kind, col, opener, onClose }: FilterDialogProps) {
  const id = useId();
  const firstRef = useRef<HTMLElement>(null);
  const { info } = useSyncExternalStore(model.subscribe, model.getState);
  const [problem, setProblem] = useState<TableFailure | null>(null);
  const [isAsking, setIsAsking] = useState(false);

  const [before] = useState(() => conditionOn(model.asked, col));
  const apply = async (condition: TableCondition | null) => {
    if (condition === before) {
      onClose();
      return;
    }
    setProblem(null);
    setIsAsking(true);
    model.clearRefused();
    const isShown = await model.ask(filteredBy(model.asked, col, condition));
    setIsAsking(false);
    if (isShown) {
      onClose();
    } else {
      // It is said here as well, where it can be put right.
      setProblem(model.state.refused);
      model.clearRefused();
    }
  };

  const column = info?.columns[col];
  const common = {
    before,
    firstRef,
    isAsking,
    onApply: (condition: TableCondition | null) => void apply(condition),
    onClose,
  };
  return (
    <Dialog
      labelledBy={id}
      initialFocus={firstRef}
      opener={opener}
      fallbackFocus={() => model.focus()}
      onClose={() => {
        if (isAsking) {
          model.cancel();
        }
        onClose();
      }}
      className="table-dialog"
      testId="table-filter"
    >
      <DialogHeader
        id={id}
        text={
          kind === "values"
            ? t("Filter {name} by values", { name: column?.name ?? "" })
            : t("Filter {name} by a condition", { name: column?.name ?? "" })
        }
        onClose={onClose}
      />
      {kind === "values" ? (
        <Values {...common} model={model} col={col} />
      ) : (
        <Condition {...common} kind={column?.type.kind ?? "text"} />
      )}
      {problem && (
        <p className="table-failure" role="alert" data-testid="table-filter-problem">
          {failureText(problem)}
        </p>
      )}
    </Dialog>
  );
}

interface PartProps {
  /** The condition there is on the column. */
  readonly before: TableCondition | null;
  readonly firstRef: RefObject<HTMLElement | null>;
  readonly isAsking: boolean;
  readonly onApply: (condition: TableCondition | null) => void;
  readonly onClose: () => void;
}

function Footer({
  before,
  isAsking,
  canApply,
  onApply,
  onClose,
}: Omit<PartProps, "firstRef"> & { readonly canApply: boolean }) {
  return (
    <div className="dialog-footer">
      {before && (
        <button
          type="button"
          className="toolbar-btn"
          data-testid="table-filter-clear"
          onClick={() => onApply(null)}
        >
          {t("Clear Filter")}
        </button>
      )}
      <button type="button" className="toolbar-btn" onClick={onClose}>
        {t("Cancel")}
      </button>
      <button
        type="submit"
        className="toolbar-btn primary-btn"
        aria-disabled={!canApply || isAsking || undefined}
        data-testid="table-filter-apply"
      >
        {isAsking ? t("Filtering…") : t("Apply|filter")}
      </button>
    </div>
  );
}

/** The values a column has, to choose from. */
function Values({
  model,
  col,
  firstRef,
  ...actions
}: PartProps & { readonly model: TableModel; readonly col: number }) {
  const { before } = actions;
  const chosenBefore = before?.kind === "oneOf" ? before : null;
  const [search, setSearch] = useState("");
  const [listed, setListed] = useState<TableValues | null>(null);
  const [problem, setProblem] = useState<TableFailure | null>(null);
  /** What was chosen, with or without blanks. All that is listed while nothing was. */
  const [chosen, setChosen] = useState(
    chosenBefore && { keys: new Set(chosenBefore.keys), blank: chosenBefore.blank },
  );

  useEffect(() => {
    const controller = new AbortController();
    const timer = window.setTimeout(
      () =>
        model.values(col, search, MOST_VALUES, controller.signal).then(
          (found) => {
            setProblem(null);
            setListed(found);
            announce(tn(found.values.length, "{n} value", "{n} values"));
          },
          (error: unknown) => {
            if (!controller.signal.aborted) {
              setProblem(failureOf(error));
            }
          },
        ),
      search === "" ? 0 : PAUSE_MS,
    );
    return () => {
      window.clearTimeout(timer);
      controller.abort();
    };
  }, [model, col, search]);

  const listedKeys = listed?.values.map(({ key }) => key) ?? [];
  const isComplete = listed?.complete === true;
  const keys = chosen?.keys ?? new Set(isComplete ? listedKeys : []);
  const blank = chosen?.blank ?? (isComplete && search === "");
  const choose = (wanted: readonly string[], isChosen: boolean, withBlank = blank) => {
    const next = new Set(keys);
    for (const key of wanted) {
      if (isChosen) {
        next.add(key);
      } else {
        next.delete(key);
      }
    }
    setChosen({ keys: next, blank: withBlank });
  };
  const isAll =
    isComplete &&
    search === "" &&
    (blank || listed.blanks === 0) &&
    listedKeys.every((key) => keys.has(key));
  const hasBlanks = (listed?.blanks ?? 0) > 0 || chosenBefore?.blank === true;
  // What is listed leaves out what other columns leave none of.
  const isSame =
    chosenBefore?.blank === blank &&
    chosenBefore.keys.length === keys.size &&
    chosenBefore.keys.every((key) => keys.has(key));

  return (
    <form
      className="table-form"
      onSubmit={(event) => {
        event.preventDefault();
        if (listed && (keys.size > 0 || blank)) {
          actions.onApply(
            isSame
              ? before
              : isAll
                ? null
                : { kind: "oneOf", keys: [...keys], blank: blank && hasBlanks },
          );
        }
      }}
    >
      <input
        ref={(input) => {
          firstRef.current = input;
        }}
        type="search"
        className="table-search"
        aria-label={t("Search values")}
        placeholder={t("Search values")}
        data-testid="table-values-search"
        value={search}
        onChange={(event) => setSearch(event.target.value)}
      />
      <div className="m3a-row">
        <button
          type="button"
          className="toolbar-btn"
          data-testid="table-values-all"
          onClick={() => {
            choose(listedKeys, true, true);
          }}
        >
          {t("Select All")}
        </button>
        <button
          type="button"
          className="toolbar-btn"
          data-testid="table-values-none"
          onClick={() => {
            choose(listedKeys, false, false);
          }}
        >
          {t("Unselect All")}
        </button>
      </div>
      <fieldset className="m3a-list table-values" aria-label={t("Values")} aria-busy={!listed}>
        {hasBlanks && (
          <label className="m3a-field">
            <input
              type="checkbox"
              data-testid="table-values-blank"
              checked={blank}
              onChange={(event) => choose([], true, event.target.checked)}
            />
            <em>{t("blank|cell")}</em>
            <span className="table-count">{fmtInt(listed?.blanks ?? 0)}</span>
          </label>
        )}
        {listed?.values.map(({ key, text, count }) => (
          <label key={key} className="m3a-field">
            <input
              type="checkbox"
              checked={keys.has(key)}
              onChange={(event) => choose([key], event.target.checked)}
            />
            <span title={text}>{text}</span>
            <span className="table-count">{fmtInt(count)}</span>
          </label>
        ))}
      </fieldset>
      {!listed && !problem && <p className="m3a-status">{t("Listing values…")}</p>}
      {listed && !listed.complete && (
        <p className="m3a-status" data-testid="table-values-more">
          {t("These are the {n} most frequent values. Search for others.", {
            n: fmtInt(listed.values.length),
          })}
        </p>
      )}
      {problem && (
        <p className="table-failure" role="alert">
          {failureText(problem)}
        </p>
      )}
      <Footer {...actions} canApply={listed !== null && (keys.size > 0 || blank)} />
    </form>
  );
}

type ConditionChoice =
  | "contains"
  | "startsWith"
  | "endsWith"
  | `number:${TableNumberOp}`
  | `time:${TableTimeOp}`
  | "largest"
  | "smallest"
  | "blank"
  | "notBlank";

function choiceText(choice: ConditionChoice, isTimeColumn: boolean): string {
  switch (choice) {
    case "contains":
      return t("contains|condition");
    case "startsWith":
      return t("begins with|condition");
    case "endsWith":
      return t("ends with|condition");
    case "number:equal":
      return t("equals");
    case "number:notEqual":
      return t("does not equal");
    case "number:greater":
      return t("is greater than");
    case "number:atLeast":
      return t("is at least");
    case "number:less":
      return t("is less than");
    case "number:atMost":
      return t("is at most");
    case "number:between":
    case "time:between":
      return t("is between");
    case "time:before":
      return t("is before");
    case "time:after":
      return t("is after");
    case "time:on":
      return t("is on the day");
    case "largest":
      return isTimeColumn ? t("is one of the latest") : t("is one of the largest");
    case "smallest":
      return isTimeColumn ? t("is one of the earliest") : t("is one of the smallest");
    case "blank":
      return t("is blank");
    case "notBlank":
      return t("is not blank");
  }
}

function choicesFor(kind: Parameters<typeof isNumeric>[0]): readonly ConditionChoice[] {
  const always = ["notBlank", "blank"] as const;
  if (isNumeric(kind)) {
    return [
      "number:equal",
      "number:notEqual",
      "number:greater",
      "number:atLeast",
      "number:less",
      "number:atMost",
      "number:between",
      "largest",
      "smallest",
      ...always,
    ];
  }
  if (isTime(kind)) {
    return [
      "time:on",
      "time:before",
      "time:after",
      "time:between",
      "largest",
      "smallest",
      ...always,
    ];
  }
  return kind === "text" ? ["contains", "startsWith", "endsWith", ...always] : always;
}

function choiceOf(condition: TableCondition): ConditionChoice | null {
  switch (condition.kind) {
    case "oneOf":
      return null;
    case "number":
      return `number:${condition.op}`;
    case "time":
      return `time:${condition.op}`;
    case "top":
      return condition.bottom ? "smallest" : "largest";
    default:
      return condition.kind;
  }
}

/** What a condition compares with. */
function thresholdsOf(condition: TableCondition | null): {
  readonly a: string;
  readonly b: string;
} {
  switch (condition?.kind) {
    case "contains":
    case "startsWith":
    case "endsWith":
      return { a: condition.text, b: "" };
    case "number":
    case "time":
      return { a: condition.a, b: condition.b ?? "" };
    case "top":
      return { a: String(condition.n), b: "" };
    default:
      return { a: "", b: "" };
  }
}

/** The condition that was put together, or `null` while something is missing. */
function conditionFrom(choice: ConditionChoice, a: string, b: string): TableCondition | null {
  const [kind, op] = choice.split(":");
  const first = a.trim();
  const second = b.trim();
  switch (choice) {
    case "blank":
    case "notBlank":
      return { kind: choice };
    case "contains":
    case "startsWith":
    case "endsWith":
      return a === "" ? null : { kind: choice, text: a };
    case "largest":
    case "smallest":
      return /^\d{1,9}$/.test(first) && Number(first) > 0
        ? { kind: "top", n: Number(first), bottom: choice === "smallest" }
        : null;
  }
  if (first === "" || (op === "between" && second === "")) {
    return null;
  }
  const thresholds = op === "between" ? { a: first, b: second } : { a: first };
  return kind === "number"
    ? { kind, op: op as TableNumberOp, ...thresholds }
    : { kind: "time", op: op as TableTimeOp, ...thresholds };
}

/** A condition, put together from what it compares with. */
function Condition({
  kind,
  firstRef,
  ...actions
}: PartProps & { readonly kind: Parameters<typeof isNumeric>[0] }) {
  const { before } = actions;
  const choices = choicesFor(kind);
  const [choice, setChoice] = useState<ConditionChoice>(
    (before && choiceOf(before)) ?? choices[0] ?? "notBlank",
  );
  const [known] = useState(() => thresholdsOf(before));
  const [a, setA] = useState(known.a);
  const [b, setB] = useState(known.b);
  const isCount = choice === "largest" || choice === "smallest";
  const isDay = choice === "time:on";
  const type = isCount
    ? "number"
    : !isTime(kind)
      ? "text"
      : kind === "dateTime" && !isDay
        ? "datetime-local"
        : "date";
  // What was typed for another condition only counts as far as it is shown.
  const shown = (value: string) => {
    const [, day, time] = /^(\d{4}-\d\d-\d\d)(?:[ T](.+))?$/.exec(value) ?? [];
    return type === "date"
      ? (day ?? "")
      : type === "datetime-local"
        ? day
          ? `${day}T${time ?? "00:00"}`
          : ""
        : value;
  };
  const condition = conditionFrom(choice, shown(a), shown(b));

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (condition) {
      actions.onApply(condition);
    }
  };
  const field = (
    label: string,
    value: string,
    onChange: (value: string) => void,
    testId: string,
  ): ReactNode => (
    <input
      // What one kind of input has is nothing to another.
      key={`${testId}${type}`}
      type={type}
      inputMode={isNumeric(kind) && !isCount ? "decimal" : undefined}
      step={type === "datetime-local" ? 1 : undefined}
      min={isCount ? 1 : undefined}
      aria-label={label}
      data-testid={testId}
      value={shown(value)}
      onChange={(event) => onChange(event.target.value)}
    />
  );
  return (
    <form className="table-form" onSubmit={submit}>
      <div className="m3a-row table-condition">
        <select
          ref={(select) => {
            firstRef.current = select;
          }}
          aria-label={t("Condition")}
          data-testid="table-condition"
          value={choice}
          onChange={(event) => setChoice(event.target.value as ConditionChoice)}
        >
          {choices.map((one) => (
            <option key={one} value={one}>
              {choiceText(one, isTime(kind))}
            </option>
          ))}
        </select>
        {choice !== "blank" && choice !== "notBlank" && field(t("Value"), a, setA, "table-value-a")}
        {choice.endsWith(":between") && (
          <>
            <span>{t("and|between")}</span>
            {field(t("Other value"), b, setB, "table-value-b")}
          </>
        )}
      </div>
      <Footer {...actions} canApply={condition !== null} />
    </form>
  );
}
