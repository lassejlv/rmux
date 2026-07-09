#!/usr/bin/env node
// Downloads the prebuilt rmux binary for this platform from GitHub Releases.
"use strict";

const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
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
  const checksumUrl = `${url}.sha256`;
  const destDir = path.join(__dirname, "bin");
  const dest = path.join(destDir, "rmux-native");

  const [response, checksumResponse] = await Promise.all([
    fetch(url, { redirect: "follow" }),
    fetch(checksumUrl, { redirect: "follow" }),
  ]);
  requireSuccessfulResponse(response, url);
  requireSuccessfulResponse(checksumResponse, checksumUrl);

  fs.mkdirSync(destDir, { recursive: true });
  const buffer = Buffer.from(await response.arrayBuffer());
  const expectedChecksum = parseChecksum(await checksumResponse.text());
  verifyChecksum(buffer, expectedChecksum);
  fs.writeFileSync(dest, buffer, { mode: 0o755 });
  console.log(`rmux ${version} installed for ${key}`);
}

function requireSuccessfulResponse(response, url) {
  if (!response.ok) {
    throw new Error(
      `download failed (${response.status}) for ${url}\nIf no release exists yet, install from source: cargo install --git https://github.com/${REPO}`,
    );
  }
}

function parseChecksum(text) {
  const match = text.match(/\b[a-fA-F0-9]{64}\b/);
  if (!match) {
    throw new Error("release checksum is missing or invalid");
  }
  return match[0].toLowerCase();
}

function verifyChecksum(buffer, expectedChecksum) {
  const actualChecksum = crypto.createHash("sha256").update(buffer).digest("hex");
  if (actualChecksum !== expectedChecksum) {
    throw new Error(
      `release checksum mismatch: expected ${expectedChecksum}, received ${actualChecksum}`,
    );
  }
  return actualChecksum;
}

function fail(message) {
  console.error(`rmux install error: ${message}`);
  process.exit(1);
}

if (require.main === module) {
  main().catch((err) => fail(err.message));
}

module.exports = { parseChecksum, verifyChecksum };
