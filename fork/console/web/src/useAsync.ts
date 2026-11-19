import { useCallback, useEffect, useState } from "react";

export type AsyncState<T> =
  | { status: "loading" }
  | { status: "done"; value: T }
  | { status: "failed"; error: unknown };

/** Run `load` when `deps` change, and again on `reload()`; a stale run never wins. */
export function useAsync<T>(load: () => Promise<T>, deps: unknown[]): { state: AsyncState<T>; reload: () => void } {
  const [state, setState] = useState<AsyncState<T>>({ status: "loading" });
  const [generation, setGeneration] = useState(0);
  const run = useCallback(load, deps);
  useEffect(() => {
    let current = true;
    setState({ status: "loading" });
    run().then(
      (value) => current && setState({ status: "done", value }),
      (error: unknown) => current && setState({ status: "failed", error }),
    );
    return () => {
      current = false;
    };
  }, [run, generation]);
  return { state, reload: () => setGeneration((value) => value + 1) };
}
