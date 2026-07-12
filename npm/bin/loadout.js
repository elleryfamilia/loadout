#!/usr/bin/env node
// npx bootstrapper for loadout. If the `load` binary is already installed,
// delegate to it. If not, explain exactly what the official installer will do,
// ask for consent (interactive terminals only), install, then delegate.
//
// This package deliberately contains no binaries and has no dependencies: it
// is an installer/launcher, not the product. The product is the `load` CLI.
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');

const INSTALLER_URL =
  'https://github.com/elleryfamilia/loadout/releases/latest/download/loadout-installer.sh';

// Install locations the official installer uses (cargo-home layout today;
// ~/.local/bin kept as a fallback in case the dist config changes).
function candidateDirs() {
  const home = os.homedir();
  const cargoHome = process.env.CARGO_HOME || path.join(home, '.cargo');
  return [path.join(cargoHome, 'bin'), path.join(home, '.local', 'bin')];
}

// PATH first; then the known install dirs, which covers the fresh-install case
// where the current shell hasn't picked up the PATH change yet.
function findLoad() {
  const which = spawnSync('which', ['load'], { encoding: 'utf8' });
  if (which.status === 0) {
    const p = which.stdout.trim().split('\n')[0];
    if (p) return p;
  }
  for (const dir of candidateDirs()) {
    const p = path.join(dir, 'load');
    if (fs.existsSync(p)) return p;
  }
  return null;
}

// Hand off to the real binary. SIGINT must reach only the child (e.g. Ctrl-C
// stopping `load studio` prints its exit message); a no-op listener keeps this
// wrapper alive until the child exits, then the child's status is mirrored.
function delegate(bin, args) {
  process.on('SIGINT', () => {});
  const r = spawnSync(bin, args, { stdio: 'inherit' });
  process.exit(r.status === null ? 1 : r.status);
}

function fail(msg) {
  process.stderr.write(`${msg}\n`);
  process.exit(1);
}

function confirm(question) {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) => {
    rl.question(question, (answer) => {
      rl.close();
      const a = answer.trim().toLowerCase();
      resolve(a === '' || a === 'y' || a === 'yes');
    });
  });
}

async function main() {
  const args = process.argv.slice(2);
  const manual = `To install manually instead:\n\n  curl -LsSf ${INSTALLER_URL} | sh\n`;

  if (process.platform === 'win32') {
    fail('loadout is unix-only today (macOS and Linux). On Windows, run it inside WSL.');
  }

  const existing = findLoad();
  if (existing) delegate(existing, args);

  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    fail(
      'loadout is not installed, and this is not an interactive terminal, ' +
        `so no consent prompt can be shown.\n${manual}`
    );
  }

  const installDir = candidateDirs()[0];
  process.stdout.write(
    `loadout is not installed yet. This runs the official installer, which:\n\n` +
      `  - downloads the prebuilt \`load\` binary for your platform (GitHub Releases)\n` +
      `  - places it in ${installDir}\n` +
      `  - adds that directory to your PATH (shell rc files) if it isn't already\n` +
      `  - writes an install receipt so \`load update\` can update it in place\n\n` +
      `Remove later with: rm ${path.join(installDir, 'load')}\n\n`
  );
  const ok = await confirm('Install load? [Y/n] ');
  if (!ok) {
    process.stdout.write(`\nNothing installed.\n${manual}`);
    process.exit(1);
  }
  process.stdout.write('\n');

  const inst = spawnSync('sh', ['-c', `curl -LsSf ${INSTALLER_URL} | sh`], { stdio: 'inherit' });
  if (inst.status !== 0) {
    fail('\nThe installer did not complete. Nothing was launched.');
  }

  const bin = findLoad();
  if (!bin) {
    fail(
      'Install finished, but the `load` binary was not found in the expected ' +
        `locations (${candidateDirs().join(', ')}). Open a new shell and try \`load --help\`.`
    );
  }

  process.stdout.write(
    `\nInstalled: ${bin}\n` +
      'From now on, use `load` directly — no npx needed. New shells have it on PATH.\n\n'
  );

  if (args.length === 0) {
    process.stdout.write(
      'Next steps:\n\n' +
        '  load studio    set up your loadout in the browser\n' +
        '  load claude    launch Claude Code with your context equipped\n' +
        '                 (also: load cursor, load codex, load gemini, load opencode)\n'
    );
    process.exit(0);
  }
  delegate(bin, args);
}

main();
