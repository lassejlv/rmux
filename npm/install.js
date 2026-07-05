#!/usr/bin/env node
// Downloads the prebuilt rmux binary for this platform from GitHub Releases.
"use strict";

const fs = require("fs");
const path = require("path");
const { version, repository } = require("./package.json");

const REPO = repository.url.match(/github\.com[/:](.+?)(?:\.git)?$/)[1];

const TARGETS = {
  "darwin-arm64": "rmux-darwin-arm64",
  "darwin-x64": "rmux-darwin-x64",
  "linux-x64": "rmux-linux-x64",
  "linux-arm64": "rmux-linux-arm64",
};

async function main() {
  const key = `${process.platform}-${process.arch}`;
  const asset = TARGETS[key];
  if (!asset) {
    fail(`unsupported platform ${key}. Install from source instead: cargo install --git https://github.com/${REPO}`);
  }

  const url = `https://github.com/${REPO}/releases/download/v${version}/${asset}`;
  const destDir = path.join(__dirname, "bin");
  const dest = path.join(destDir, "rmux-native");

  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    fail(`download failed (${response.status}) for ${url}\nIf no release exists yet, install from source: cargo install --git https://github.com/${REPO}`);
  }

  fs.mkdirSync(destDir, { recursive: true });
  const buffer = Buffer.from(await response.arrayBuffer());
  fs.writeFileSync(dest, buffer, { mode: 0o755 });
  console.log(`rmux ${version} installed for ${key}`);
}

function fail(message) {
  console.error(`rmux install error: ${message}`);
  process.exit(1);
}

main().catch((err) => fail(err.message));
