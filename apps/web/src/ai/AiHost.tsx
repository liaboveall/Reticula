import { useEffect } from "react";
import { withPersonTexts } from "../collab/labels.ts";
import type { EditorHandle } from "../commands/handle.ts";
import { isTouchMode } from "../editor/touch-mode.ts";
import AiPanel, { type AiPanelProps } from "./AiPanel.tsx";

type AiHostProps = Pick<
  AiPanelProps,
  "open" | "opener" | "onFocusGrid" | "activeSheetId" | "draftActive" | "onClose"
> & {
  readonly handle: EditorHandle;
};

/** The panel of the analyst, in the editor. */
export default function AiHost({ handle, ...props }: AiHostProps) {
  const collab = handle.collab();
  const readOnly = isTouchMode();
  // A sheet that is opened has nobody in it, and those who are told of once.
  const { draftActive } = props;
  useEffect(() => {
    if (!draftActive) {
      handle.grid()?.setRemotePresences(withPersonTexts(handle.presences()));
    }
  }, [draftActive, handle]);

  return (
    <AiPanel
      {...props}
      collabDocId={collab?.docId ?? null}
      canWriteCloud={collab !== null && collab.role !== "viewer" && !readOnly}
      readOnly={readOnly}
      onOpenCloudDraft={(docId, branchId) =>
        handle.sendHost({ kind: "draftOpenCloud", docId, branchId })
      }
      onOpenReplayDraft={(baseSnapshot) =>
        handle.sendHost({ kind: "draftOpenReplay", baseSnapshot }, [baseSnapshot])
      }
      onApplyReplayTxn={(txn) => handle.sendHost({ kind: "draftApplyTxn", txn }, [txn])}
      onCommitLocalMerge={(title, baseSnapshot, txns) => {
        handle.noteChange();
        handle.sendHost(
          {
            kind: "draftCommitLocalMerge",
            docId: handle.freshDocId(),
            title,
            baseSnapshot,
            txns,
          },
          [baseSnapshot, ...txns],
        );
      }}
      onExitDraft={() => handle.sendHost({ kind: "draftExit" })}
      onSetAiPresence={(presence) =>
        handle
          .grid()
          ?.setRemotePresences(
            withPersonTexts([...handle.presences(), ...(presence ? [presence] : [])]),
          )
      }
      onSetDiffHighlights={(map) => handle.grid()?.setDiffHighlights(map)}
      onJumpToCell={(sheet, row, col) =>
        handle.onSheet(sheet, (grid) => grid.jumpTo(row, col, false))
      }
    />
  );
}
