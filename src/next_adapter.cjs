'use strict';

const fs = require('node:fs');
const path = require('node:path');

function relativeToProject(projectDir, filePath) {
  if (!filePath) return null;
  const relative = path.relative(projectDir, filePath);
  if (!relative || relative.startsWith('..') || path.isAbsolute(relative)) {
    throw new Error(`adapter output escapes project directory: ${filePath}`);
  }
  return relative.split(path.sep).join('/');
}

function compactOutput(projectDir, output) {
  if (!output) return null;
  return {
    type: output.type,
    id: output.id,
    pathname: output.pathname,
    runtime: output.runtime || 'nodejs',
    filePath: relativeToProject(projectDir, output.filePath),
    assets: Object.fromEntries(
      Object.entries(output.assets || {}).map(([logical, absolute]) => [
        logical,
        relativeToProject(projectDir, absolute),
      ]),
    ),
  };
}

const adapter = {
  name: 'meshscale',
  async onBuildComplete({
    routing,
    outputs,
    projectDir,
    config,
    nextVersion,
    buildId,
  }) {
    const all = [
      ...(outputs.pages || []),
      ...(outputs.pagesApi || []),
      ...(outputs.appPages || []),
      ...(outputs.appRoutes || []),
    ];

    for (const output of all) {
      if (output.runtime && output.runtime !== 'nodejs') {
        throw new Error(
          `MeshScale currently supports Node.js adapter outputs only: ${output.pathname}`,
        );
      }
    }

    if (outputs.middleware) {
      throw new Error(
        'MeshScale adapter runtime does not support middleware/proxy yet; this deployment requires middleware execution.',
      );
    }

    const metadata = {
      version: 1,
      nextVersion,
      buildId,
      config: {
        basePath: config.basePath || '',
        i18n: config.i18n || null,
      },
      routing,
      outputs: {
        pages: (outputs.pages || []).map((output) =>
          compactOutput(projectDir, output),
        ),
        pagesApi: (outputs.pagesApi || []).map((output) =>
          compactOutput(projectDir, output),
        ),
        appPages: (outputs.appPages || []).map((output) =>
          compactOutput(projectDir, output),
        ),
        appRoutes: (outputs.appRoutes || []).map((output) =>
          compactOutput(projectDir, output),
        ),
        staticFiles: (outputs.staticFiles || []).map((output) =>
          compactOutput(projectDir, output),
        ),
        prerenders: (outputs.prerenders || []).map((output) => ({
          type: output.type,
          id: output.id,
          pathname: output.pathname,
          parentOutputId: output.parentOutputId,
          route: output.route,
        })),
      },
    };

    const metadataPath = path.join(projectDir, '.next', 'meshscale-adapter.json');
    fs.writeFileSync(metadataPath, JSON.stringify(metadata, null, 2));
  },
};

module.exports = adapter;
