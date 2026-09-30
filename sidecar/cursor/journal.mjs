// journal.mjs:sessions.jsonl 追加写、启动重放、compact 原子重写。
// 每行完整 session snapshot;追加后 fsync;截断尾行容忍,其余损坏行报错。
import { mkdir, open, readFile, rename } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

export class Journal {
  /** @param {string} path journal 文件绝对路径 */
  constructor(path) {
    this.path = path;
  }

  /** 追加一行完整 snapshot,写入后 fsync。 */
  async append(snapshot) {
    const handle = await open(this.path, "a", 0o600);
    try {
      await handle.write(`${JSON.stringify(snapshot)}\n`);
      await handle.sync();
    } finally {
      await handle.close();
    }
  }

  /**
   * 重放 journal:按行序返回 snapshot 数组,不去重(调用方按 sessionKey 取最后)。
   * 文件缺失返回空数组;最后一行无法解析视为截断丢弃;其余损坏行抛错。
   */
  async replay() {
    let text;
    try {
      text = await readFile(this.path, "utf8");
    } catch (error) {
      if (error.code === "ENOENT") return [];
      throw error;
    }
    const lines = text.split("\n");
    while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
    const records = [];
    for (let index = 0; index < lines.length; index += 1) {
      try {
        records.push(JSON.parse(lines[index]));
      } catch (error) {
        if (index === lines.length - 1) break;
        throw new Error(`journal line ${index + 1} is corrupt: ${lines[index].slice(0, 80)}`);
      }
    }
    return records;
  }

  /**
   * compact:临时文件写完、fsync、原子 rename,再 sync 父目录。
   * @param {Array} records 启动筛选后的最新快照(已按保留顺序)
   */
  async compact(records) {
    const dir = dirname(this.path);
    // auth directory 可能尚未创建(手工配置 cursor_auth_dir 指向新路径)
    await mkdir(dir, { recursive: true, mode: 0o700 });
    const tmp = join(dir, `.${basename(this.path)}.tmp-${process.pid}-${Date.now()}`);
    const handle = await open(tmp, "w", 0o600);
    try {
      for (const record of records) {
        await handle.write(`${JSON.stringify(record)}\n`);
      }
      await handle.sync();
    } finally {
      await handle.close();
    }
    await rename(tmp, this.path);
    const dirHandle = await open(dir, "r");
    try {
      await dirHandle.sync();
    } finally {
      await dirHandle.close();
    }
  }
}
