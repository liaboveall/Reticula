/**
 * Something for `lazy` that is there already. A promise would have it shown a
 * moment later, with nothing in its place until then.
 */
export function resolved<T>(value: T): Promise<T> {
  // biome-ignore lint/suspicious/noThenProperty: that is what makes it something to wait for
  const thenable = { then: (accept: (value: T) => void) => accept(value) };
  // It is only ever waited for in this way.
  return thenable as unknown as Promise<T>;
}
