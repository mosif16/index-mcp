import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export interface PackageMetadata {
  name: string;
  version: string;
  description?: string;
}

let cachedMetadata: PackageMetadata | null = null;

function readPackageJson(): PackageMetadata {
  const currentDir = path.dirname(fileURLToPath(import.meta.url));
  const packagePath = path.join(currentDir, '..', 'package.json');
  const raw = fs.readFileSync(packagePath, 'utf8');
  const parsed = JSON.parse(raw) as PackageMetadata;
  if (!parsed.name || !parsed.version) {
    throw new Error(`package.json at ${packagePath} is missing required fields.`);
  }
  return parsed;
}

export function getPackageMetadata(): PackageMetadata {
  if (!cachedMetadata) {
    cachedMetadata = readPackageJson();
  }
  return cachedMetadata;
}
