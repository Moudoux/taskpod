// Which task files include which, across the workspace, so an included file
// can be read together with the files that include it. No vscode imports.

export class IncludeGraph {
  /** Each file's resolved `include` paths. */
  private includes = new Map<string, string[]>();

  /** Records a file's includes; true if they changed. */
  set(file: string, includes: string[]): boolean {
    const old = this.includes.get(file);
    this.includes.set(file, includes);
    return !old || old.length !== includes.length || old.some((p, i) => p !== includes[i]);
  }

  /** Forgets a file; true if it included anything. */
  delete(file: string): boolean {
    const had = (this.includes.get(file)?.length ?? 0) > 0;
    this.includes.delete(file);
    return had;
  }

  /** Files that include `file` directly. */
  includers(file: string): string[] {
    const found: string[] = [];
    for (const [from, targets] of this.includes) if (from !== file && targets.includes(file)) found.push(from);
    return found.sort();
  }

  /**
   * The top-level files that include `file`, directly or through other includes:
   * the files taskpod would be run on. Empty when nothing includes it. In a cycle
   * with no file above it, every file in the cycle counts.
   */
  roots(file: string): string[] {
    const seen = new Set([file]);
    const roots: string[] = [];
    const queue = this.includers(file);
    const ancestors: string[] = [];
    while (queue.length) {
      const next = queue.shift()!;
      if (seen.has(next)) continue;
      seen.add(next);
      ancestors.push(next);
      const above = this.includers(next);
      if (!above.length) roots.push(next);
      queue.push(...above);
    }
    return (roots.length ? roots : ancestors).sort();
  }
}
