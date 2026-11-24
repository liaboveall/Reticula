import type { EditorHandle } from "../commands/handle.ts";
import CollabShell, { type CollabShellProps } from "./CollabShell.tsx";

type CollabHostProps = Omit<
  CollabShellProps,
  | "onOpenCollab"
  | "onUpdateVisibility"
  | "onOpenLocalRescue"
  | "onSwitchToLocal"
  | "onHistorySeek"
  | "onHistoryExit"
> & {
  readonly handle: EditorHandle;
  /** The workbook to go back to from the cloud. */
  readonly localDocId: string;
  readonly onRescued: () => void;
};

/** What there is of collaboration around the grid, in the editor. */
export default function CollabHost({ handle, localDocId, onRescued, ...props }: CollabHostProps) {
  return (
    <CollabShell
      {...props}
      onOpenCollab={(target) => handle.openCollab(target)}
      onUpdateVisibility={(visibility) => handle.updateCollab({ visibility })}
      onOpenLocalRescue={(docId, title) => {
        onRescued();
        handle.openWith(docId, { kind: "openDoc", docId, title }, true);
      }}
      onSwitchToLocal={() =>
        handle.openWith(localDocId, { kind: "openDoc", docId: localDocId }, true)
      }
      onHistorySeek={(targetSeq, snapshotSeq) => {
        const collab = handle.collab();
        if (collab) {
          const { docId, shareToken } = collab;
          handle.sendHost({ kind: "historySeek", docId, targetSeq, snapshotSeq, shareToken });
        }
      }}
      onHistoryExit={() => handle.sendHost({ kind: "historyExit" })}
    />
  );
}
