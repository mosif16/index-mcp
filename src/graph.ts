import crypto from 'node:crypto';
import path from 'node:path';
import ts from 'typescript';

export type GraphNodeKind = 'file' | 'function' | 'class' | 'method' | 'module' | 'symbol';
export type GraphEdgeType = 'imports' | 'calls';

export interface GraphEntity {
  id: string;
  path: string | null;
  kind: GraphNodeKind;
  name: string;
  signature?: string | null;
  rangeStart?: number;
  rangeEnd?: number;
  metadata?: Record<string, unknown> | null;
}

export interface GraphEdge {
  id: string;
  sourceId: string;
  targetId: string;
  type: GraphEdgeType;
  sourcePath: string | null;
  targetPath: string | null;
  metadata?: Record<string, unknown> | null;
}

export interface GraphExtractionResult {
  entities: GraphEntity[];
  edges: GraphEdge[];
}

const SUPPORTED_EXTENSIONS = new Set(['.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs']);

function stableId(parts: string[]): string {
  const hash = crypto.createHash('sha256');
  for (const part of parts) {
    hash.update(part);
    hash.update('|');
  }
  return hash.digest('hex');
}

function getLineInfo(source: ts.SourceFile, node: ts.Node) {
  const start = source.getLineAndCharacterOfPosition(node.getStart(source));
  const end = source.getLineAndCharacterOfPosition(node.getEnd());
  return {
    startLine: start.line + 1,
    startColumn: start.character + 1,
    endLine: end.line + 1,
    endColumn: end.character + 1
  };
}

function createFileEntity(relativePath: string): GraphEntity {
  return {
    id: stableId(['file', relativePath]),
    path: relativePath,
    kind: 'file',
    name: relativePath
  };
}

function functionSignature(node: ts.FunctionLikeDeclarationBase, name: string): string {
  const params = node.parameters
    .map((param) => {
      if (ts.isIdentifier(param.name)) {
        return param.name.text;
      }
      return param.getText();
    })
    .join(', ');
  return `${name}(${params})`;
}

function resolveImportPath(fromPath: string, specifier: string): string | null {
  if (!specifier.startsWith('.') && !specifier.startsWith('/')) {
    return null;
  }
  const fromDir = path.posix.dirname(fromPath);
  const resolved = path.posix.normalize(path.posix.join(fromDir, specifier));
  return resolved;
}

export function extractGraphMetadata(relativePath: string, content: string): GraphExtractionResult | null {
  const ext = path.extname(relativePath);
  if (!SUPPORTED_EXTENSIONS.has(ext)) {
    return null;
  }

  const sourceFile = ts.createSourceFile(relativePath, content, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

  const entitiesMap = new Map<string, GraphEntity>();
  const edgesMap = new Map<string, GraphEdge>();
  const fileEntity = createFileEntity(relativePath);
  entitiesMap.set(fileEntity.id, fileEntity);

  const localSymbolIndex = new Map<string, string>();
  const scopeStack: { entityId: string; path: string | null }[] = [{ entityId: fileEntity.id, path: relativePath }];
  const classStack: { entityId: string; name: string }[] = [];

  function registerEntity(key: string, factory: () => GraphEntity): GraphEntity {
    if (entitiesMap.has(key)) {
      return entitiesMap.get(key)!;
    }
    const entity = factory();
    entitiesMap.set(key, entity);
    return entity;
  }

  function registerEdge(type: GraphEdgeType, sourceId: string, targetId: string, metadata: Record<string, unknown> | null, sourcePath: string | null, targetPath: string | null) {
    const id = stableId(['edge', type, sourceId, targetId]);
    if (edgesMap.has(id)) {
      return;
    }
    edgesMap.set(id, {
      id,
      sourceId,
      targetId,
      type,
      sourcePath,
      targetPath,
      metadata
    });
  }

  function enterScope(entity: GraphEntity) {
    scopeStack.push({ entityId: entity.id, path: entity.path });
  }

  function exitScope() {
    scopeStack.pop();
  }

  function currentScope(): { entityId: string; path: string | null } {
    return scopeStack[scopeStack.length - 1];
  }

  function ensureLocalSymbol(name: string, entity: GraphEntity): void {
    localSymbolIndex.set(name, entity.id);
  }

  function getOrCreateSymbolEntity(name: string): GraphEntity {
    const localId = localSymbolIndex.get(name);
    if (localId) {
      return entitiesMap.get(localId)!;
    }
    const key = stableId(['symbol', name]);
    return registerEntity(key, () => ({
      id: key,
      path: null,
      kind: 'symbol',
      name
    }));
  }

  function handleCallExpression(node: ts.CallExpression) {
    const scope = currentScope();
    const expression = node.expression;
    let targetName: string | null = null;

    if (ts.isIdentifier(expression)) {
      targetName = expression.text;
    } else if (ts.isPropertyAccessExpression(expression)) {
      targetName = expression.name.text;
    }

    if (!targetName) {
      return;
    }

    const targetEntity = getOrCreateSymbolEntity(targetName);
    const lineInfo = getLineInfo(sourceFile, node);
    const metadata = {
      target: targetName,
      startLine: lineInfo.startLine,
      startColumn: lineInfo.startColumn,
      endLine: lineInfo.endLine,
      endColumn: lineInfo.endColumn
    };

    registerEdge('calls', scope.entityId, targetEntity.id, metadata, scope.path, targetEntity.path);
  }

  function withFunctionEntity(
    node: ts.FunctionLikeDeclarationBase,
    name: string,
    kind: GraphNodeKind,
    metadata: Record<string, unknown> | null,
    visitChildren: () => void
  ) {
    const signature = functionSignature(node, name);
    const entityId = stableId([kind, relativePath, name, node.pos.toString(), node.end.toString()]);
    const lineInfo = getLineInfo(sourceFile, node);
    const entity = registerEntity(entityId, () => ({
      id: entityId,
      path: relativePath,
      kind,
      name,
      signature,
      rangeStart: node.getStart(sourceFile),
      rangeEnd: node.getEnd(),
      metadata: {
        ...(metadata ?? {}),
        startLine: lineInfo.startLine,
        startColumn: lineInfo.startColumn,
        endLine: lineInfo.endLine,
        endColumn: lineInfo.endColumn
      }
    }));

    ensureLocalSymbol(name, entity);
    enterScope(entity);
    visitChildren();
    exitScope();
  }

  function visit(node: ts.Node): void {
    if (ts.isImportDeclaration(node)) {
      if (!ts.isStringLiteral(node.moduleSpecifier)) {
        ts.forEachChild(node, visit);
        return;
      }
      const specifier = node.moduleSpecifier.text;
      const resolved = resolveImportPath(relativePath, specifier);
      const importMetadata = {
        specifier,
        resolvedPath: resolved,
        namedImports: node.importClause?.namedBindings && ts.isNamedImports(node.importClause.namedBindings)
          ? node.importClause.namedBindings.elements.map((el) => el.name.text)
          : [],
        defaultImport: node.importClause?.name ? node.importClause.name.text : null,
        namespaceImport:
          node.importClause?.namedBindings && ts.isNamespaceImport(node.importClause.namedBindings)
            ? node.importClause.namedBindings.name.text
            : null
      };
      const moduleId = stableId(['module', specifier, resolved ?? '']);
      const moduleEntity = registerEntity(moduleId, () => ({
        id: moduleId,
        path: resolved ?? null,
        kind: 'module',
        name: specifier,
        metadata: resolved ? { resolvedPath: resolved } : null
      }));
      registerEdge('imports', currentScope().entityId, moduleEntity.id, importMetadata, relativePath, moduleEntity.path);
      ts.forEachChild(node, visit);
      return;
    }

    if (ts.isClassDeclaration(node) && node.name) {
      const className = node.name.text;
      const lineInfo = getLineInfo(sourceFile, node);
      const classId = stableId(['class', relativePath, className, node.pos.toString(), node.end.toString()]);
      const classEntity = registerEntity(classId, () => ({
        id: classId,
        path: relativePath,
        kind: 'class',
        name: className,
        signature: className,
        rangeStart: node.getStart(sourceFile),
        rangeEnd: node.getEnd(),
        metadata: {
          startLine: lineInfo.startLine,
          startColumn: lineInfo.startColumn,
          endLine: lineInfo.endLine,
          endColumn: lineInfo.endColumn
        }
      }));
      ensureLocalSymbol(className, classEntity);
      enterScope(classEntity);
      classStack.push({ entityId: classEntity.id, name: className });
      ts.forEachChild(node, visit);
      classStack.pop();
      exitScope();
      return;
    }

    if (ts.isMethodDeclaration(node) && node.name && ts.isIdentifier(node.name)) {
      const methodName = node.name.text;
      const currentClass = classStack[classStack.length - 1];
      const metadata = currentClass ? { className: currentClass.name } : null;
      withFunctionEntity(node, methodName, 'method', metadata, () => {
        ts.forEachChild(node, visit);
      });
      return;
    }

    if (ts.isConstructorDeclaration(node)) {
      const currentClass = classStack[classStack.length - 1];
      const name = currentClass ? `${currentClass.name}#constructor` : 'constructor';
      const metadata = currentClass ? { className: currentClass.name } : null;
      withFunctionEntity(node, name, 'method', metadata, () => {
        ts.forEachChild(node, visit);
      });
      return;
    }

    if (ts.isFunctionDeclaration(node) && node.name) {
      withFunctionEntity(node, node.name.text, 'function', null, () => {
        ts.forEachChild(node, visit);
      });
      return;
    }

    if ((ts.isFunctionExpression(node) || ts.isArrowFunction(node)) && ts.isVariableDeclaration(node.parent) && ts.isIdentifier(node.parent.name)) {
      const varName = node.parent.name.text;
      withFunctionEntity(node, varName, 'function', null, () => {
        ts.forEachChild(node, visit);
      });
      return;
    }

    if (ts.isCallExpression(node)) {
      handleCallExpression(node);
      ts.forEachChild(node, visit);
      return;
    }

    ts.forEachChild(node, visit);
  }

  visit(sourceFile);

  return {
    entities: [...entitiesMap.values()],
    edges: [...edgesMap.values()]
  };
}
