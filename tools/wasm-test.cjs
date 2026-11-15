// Portable scenario runner for liquidwar-rs.
//
// Builds the `wasmtest` crate for wasm32-unknown-unknown and runs the
// lwcore scenario suite inside plain WebAssembly (no WASI needed).
//
// Usage: node tools/wasm-test.cjs

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

(async () => {
  const root = path.resolve(__dirname, "..");

  execFileSync(
    "cargo",
    [
      "build",
      "-p",
      "wasmtest",
      "--target",
      "wasm32-unknown-unknown",
      "--release",
    ],
    { cwd: root, stdio: "inherit" }
  );

  const wasmPath = path.join(
    root,
    "target",
    "wasm32-unknown-unknown",
    "release",
    "wasmtest.wasm"
  );
  const bytes = fs.readFileSync(wasmPath);

  const { instance } = await WebAssembly.instantiate(bytes, {});
  const api = instance.exports;

  api.run_tests();

  const len = api.failures_len();
  const ptr = api.failures_ptr();
  if (len === 0 || ptr === 0) {
    console.error("wasm-test: no report produced (harness bug?)");
    process.exit(1);
  }
  const mem = new Uint8Array(api.memory.buffer, ptr, len);
  const text = new TextDecoder().decode(mem);

  if (text === "ALL SCENARIOS PASSED") {
    console.log("wasm-test: " + text);
    process.exit(0);
  }
  console.error("wasm-test FAILURES:\n" + text);
  process.exit(1);
})().catch((err) => {
  console.error("wasm-test: " + (err && err.message ? err.message : err));
  process.exit(1);
});
