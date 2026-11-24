import type { Provenance } from "../generated/benchmarks.ts";

export const REPOSITORY = "https://github.com/liaboveall/Reticula";

/** Attributes of a link that leaves the app. */
export const OUTSIDE = { target: "_blank", rel: "noreferrer" } as const;

export const commitUrl = ({ commit }: Provenance) => `${REPOSITORY}/commit/${commit}`;

/** Where the repository shows a file or a directory. */
export const sourceUrl = (path: string) => `${REPOSITORY}/tree/main/${path}`;
