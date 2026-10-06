#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const { createRequire } = require("node:module");

function fail(message) {
  console.error(message);
  process.exit(1);
}

const projectRoot = process.argv[2];
const entrypoint = process.argv[3];

if (!projectRoot || !entrypoint) {
  fail("usage: trace-runtime.cjs <project-root> <next-package-json>");
}

const absoluteProjectRoot = path.resolve(projectRoot);
const absoluteEntrypoint = path.resolve(entrypoint);

if (!fs.existsSync(absoluteProjectRoot)) {
  fail(`project root does not exist: ${absoluteProjectRoot}`);
}

if (!fs.existsSync(absoluteEntrypoint)) {
  fail(`Next.js package.json does not exist: ${absoluteEntrypoint}`);
}

let nodeFileTrace;
try {
  const requireFromProject = createRequire(path.join(absoluteProjectRoot, "package.json"));
  ({ nodeFileTrace } = requireFromProject("@vercel/nft"));
} catch (error) {
  fail(
    "unable to load @vercel/nft from the built project's dependency tree. " +
      "Next.js normally provides this dependency; install a compatible Next.js version " +
      "or make @vercel/nft available in the project. " +
      `Original error: ${error instanceof Error ? error.message : String(error)}`,
  );
}

const nextPackage = JSON.parse(fs.readFileSync(absoluteEntrypoint, "utf8"));
const serverEntrypoint = path.join(
  path.dirname(absoluteEntrypoint),
  "dist",
  "server",
  "next-server.js",
);

if (!fs.existsSync(serverEntrypoint)) {
  fail(`Next.js production server entrypoint does not exist: ${serverEntrypoint}`);
}

(async () => {
  try {
    const result = await nodeFileTrace([serverEntrypoint], {
      base: absoluteProjectRoot,
      processCwd: absoluteProjectRoot,
      ts: true,
      mixedModules: true,
    });

    const files = Array.from(result.fileList)
      .map((file) => file.replaceAll(path.sep, "/"))
      .sort();

    process.stdout.write(
      JSON.stringify({
        version: 1,
        nextVersion: nextPackage.version,
        files,
      }),
    );
  } catch (error) {
    fail(
      `@vercel/nft failed: ${error instanceof Error ? error.stack || error.message : String(error)}`,
    );
  }
})();
