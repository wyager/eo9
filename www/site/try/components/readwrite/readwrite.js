"use components";
export function instantiate(getCoreModule, imports, instantiateCore = WebAssembly.instantiate) {
  
  function promiseWithResolvers() {
    if (Promise.withResolvers) {
      return Promise.withResolvers();
    } else {
      let resolve;
      let reject;
      const promise = new Promise((res, rej) => {
        resolve = res;
        reject = rej;
      });
      return { promise, resolve, reject };
    }
  }
  const symbolDispose = Symbol.dispose || Symbol.for('dispose');
  const symbolAsyncIterator = Symbol.asyncIterator;
  const symbolIterator = Symbol.iterator;
  
  const _debugLog = (...args) => {
    if (!globalThis?.process?.env?.JCO_DEBUG) { return; }
    console.debug(...args);
  };
  const ASYNC_DETERMINISM = 'random';
  const GLOBAL_COMPONENT_MEMORY_MAP = new Map();
  const CURRENT_TASK_META = {};
  
  function _getGlobalCurrentTaskMeta(componentIdx) {
    const v = CURRENT_TASK_META[componentIdx];
    if (v === undefined || v === null) { return undefined; }
    return { ...v };
  }
  
  
  function _setGlobalCurrentTaskMeta(args) {
    if (!args) { throw new TypeError('args missing'); }
    if (args.taskID === undefined) { throw new TypeError('missing task ID'); }
    if (args.componentIdx === undefined) { throw new TypeError('missing component idx'); }
    const { taskID, componentIdx } = args;
    return CURRENT_TASK_META[componentIdx] = { taskID, componentIdx };
  }
  
  
  function _withGlobalCurrentTaskMeta(args) {
    _debugLog('[_withGlobalCurrentTaskMeta()] args', args);
    if (!args) { throw new TypeError('args missing'); }
    if (args.taskID === undefined) { throw new TypeError('missing task ID'); }
    if (args.componentIdx === undefined) { throw new TypeError('missing component idx'); }
    if (!args.fn) { throw new TypeError('missing fn'); }
    const { taskID, componentIdx, fn } = args;
    
    try {
      CURRENT_TASK_META[componentIdx] = { taskID, componentIdx };
      return fn();
    } catch (err) {
      _debugLog("error while executing sync callee/callback", {
        ...args,
        err,
      });
      throw err;
    } finally {
      CURRENT_TASK_META[componentIdx] = null;
    }
  }
  
  async function _withGlobalCurrentTaskMetaAsync(args) {
    _debugLog('[_withGlobalCurrentTaskMetaAsync()] args', args);
    if (!args) { throw new TypeError('args missing'); }
    if (args.taskID === undefined) { throw new TypeError('missing task ID'); }
    if (args.componentIdx === undefined) { throw new TypeError('missing component idx'); }
    if (!args.fn) { throw new TypeError('missing fn'); }
    const { taskID, componentIdx, fn } = args;
    
    // If there is already an async task executing, we must wait for it
    // to complete before we can can run the closure we were given
    //
    let current = CURRENT_TASK_META[componentIdx];
    let cstate;
    if (current && current.taskID !== taskID) {
      cstate = getOrCreateAsyncState(componentIdx);
      while (current && current.taskID !== taskID) {
        const { promise, resolve } = Promise.withResolvers();
        cstate.onNextExclusiveRelease(resolve);
        await promise;
        current = CURRENT_TASK_META[componentIdx];
      }
      
      // Since we've just waited for the component to not be locked, re-lock
      // exclusivity so we can run the fn below (likely a callee/callback)
      cstate.exclusiveLock();
    }
    
    try {
      CURRENT_TASK_META[componentIdx] = { taskID, componentIdx };
      return await fn();
    } catch (err) {
      _debugLog("error while executing async callee/callback", {
        ...args,
        err,
      });
      throw err;
    } finally {
      CURRENT_TASK_META[componentIdx] = null;
    }
  }
  
  async function _clearCurrentTask(args) {
    _debugLog('[_clearCurrentTask()] args', args);
    if (!args) { throw new TypeError('args missing'); }
    if (args.taskID === undefined) { throw new TypeError('missing task ID'); }
    if (args.componentIdx === undefined) { throw new TypeError('missing component idx'); }
    const { taskID, componentIdx } = args;
    
    const meta = CURRENT_TASK_META[componentIdx];
    if (!meta) { throw new Error(`missing current task meta for component idx [${componentIdx}]n`); }
    
    if (meta.taskID !== taskID) {
      throw new Error(`task ID [${meta.taskID}] != requested ID [${taskID}]`);
    }
    if (meta.componentIdx !== componentIdx) {
      throw new Error(`component idx [${meta.componentIdx}] != requested idx [${componentIdx}]`);
    }
    
    CURRENT_TASK_META[componentIdx] = null;
  }
  
  function lookupMemoriesForComponent(args) {
    const { componentIdx } = args ?? {};
    if (args.componentIdx === undefined) { throw new TypeError("missing component idx"); }
    
    const metas = GLOBAL_COMPONENT_MEMORY_MAP.get(componentIdx);
    if (!metas) { return []; }
    
    if (args.memoryIdx === undefined) {
      return Object.values(metas);
    }
    
    const meta = metas[args.memoryIdx];
    return meta?.memory;
  }
  
  function registerGlobalMemoryForComponent(args) {
    const { componentIdx, memory, memoryIdx } = args ?? {};
    if (componentIdx === undefined) { throw new TypeError('missing component idx'); }
    if (memory === undefined && memoryIdx === undefined) { throw new TypeError('missing both memory & memory idx'); }
    let inner = GLOBAL_COMPONENT_MEMORY_MAP.get(componentIdx);
    if (!inner) {
      inner = {};
      GLOBAL_COMPONENT_MEMORY_MAP.set(componentIdx, inner);
    }
    
    inner[memoryIdx] = { memory, memoryIdx, componentIdx };
  }
  
  class RepTable {
    #data = [0, null];
    #target;
    
    constructor(args) {
      this.target = args?.target;
    }
    
    data() { return this.#data; }
    
    insert(val) {
      _debugLog('[RepTable#insert()] args', { val, target: this.target });
      const freeIdx = this.#data[0];
      if (freeIdx === 0) {
        this.#data.push(val);
        this.#data.push(null);
        const rep = (this.#data.length >> 1) - 1;
        _debugLog('[RepTable#insert()] inserted', { val, target: this.target, rep });
        return rep;
      }
      this.#data[0] = this.#data[freeIdx << 1];
      const placementIdx = freeIdx << 1;
      this.#data[placementIdx] = val;
      this.#data[placementIdx + 1] = null;
      _debugLog('[RepTable#insert()] inserted', { val, target: this.target, rep: freeIdx });
      return freeIdx;
    }
    
    get(rep) {
      _debugLog('[RepTable#get()] args', { rep, target: this.target });
      if (rep === 0) { throw new Error('invalid resource rep during get, (cannot be 0)'); }
      
      const baseIdx = rep << 1;
      const val = this.#data[baseIdx];
      return val;
    }
    
    contains(rep) {
      _debugLog('[RepTable#contains()] args', { rep, target: this.target });
      if (rep === 0) { throw new Error('invalid resource rep during contains, (cannot be 0)'); }
      
      const baseIdx = rep << 1;
      return !!this.#data[baseIdx];
    }
    
    remove(rep) {
      _debugLog('[RepTable#remove()] args', { rep, target: this.target });
      if (rep === 0) { throw new Error('invalid resource rep during remove, (cannot be 0)'); }
      if (this.#data.length === 2) { throw new Error('invalid'); }
      
      const baseIdx = rep << 1;
      const val = this.#data[baseIdx];
      
      this.#data[baseIdx] = this.#data[0];
      this.#data[0] = rep;
      
      return val;
    }
    
    clear() {
      _debugLog('[RepTable#clear()] args', { rep, target: this.target });
      this.#data = [0, null];
    }
  }
  const _coinFlip = () => { return Math.random() > 0.5; };
  let SCOPE_ID = 0;
  const I32_MIN = -2_147_483_648;
  
  const I32_MAX= 2_147_483_647;
  
  
  function _isValidNumericPrimitive(ty, v) {
    if (v === undefined || v === null) { return false; }
    switch (ty) {
      case 'bool':
      return v === 0 || v === 1;
      break;
      case 'u8':
      return v >= 0 && v <= 255;
      break;
      case 's8':
      return v >= -128 && v <= 127;
      break;
      case 'u16':
      return v >= 0 && v <= 65535;
      break;
      case 's16':
      return v >= -32768 && v <= 32767;
      case 'u32':
      return v >= 0 && v <= 4_294_967_295;
      case 's32':
      return v >= -2_147_483_648 && v <= 2_147_483_647;
      case 'u64':
      return typeof v === 'bigint' && v >= 0 && v <= 18_446_744_073_709_551_615n;
      case 's64':
      return typeof v === 'bigint' && v >= -9223372036854775808n && v <= 9223372036854775807n;
      break;
      case 'f32':
      case 'f64': return typeof v === 'number';
      default:
      return false;
    }
    return true;
  }
  
  function _requireValidNumericPrimitive(ty, v) {
    if (v === undefined  || v === null || !_isValidNumericPrimitive(ty, v)) {
      throw new TypeError(`invalid ${ty} value [${v}]`);
    }
    return true;
  }
  
  const _typeCheckValidI32 = (n) => typeof n === 'number' && n >= I32_MIN && n <= I32_MAX;
  
  
  const _typeCheckAsyncFn= (f) => {
    return f instanceof ASYNC_FN_CTOR;
  };
  
  let RESOURCE_CALL_BORROWS = [];const ASYNC_FN_CTOR = (async () => {}).constructor;
  
  function clearCurrentTask(componentIdx, taskID) {
    _debugLog('[clearCurrentTask()] args', { componentIdx, taskID });
    
    if (componentIdx === undefined || componentIdx === null) {
      throw new Error('missing/invalid component instance index while ending current task');
    }
    
    const tasks = ASYNC_TASKS_BY_COMPONENT_IDX.get(componentIdx);
    if (!tasks || !Array.isArray(tasks)) {
      throw new Error('missing/invalid tasks for component instance while ending task');
    }
    if (tasks.length == 0) {
      throw new Error(`no current tasks for component instance [${componentIdx}] while ending task`);
    }
    
    if (taskID !== undefined) {
      const last = tasks[tasks.length - 1];
      if (last.id !== taskID) {
        // throw new Error('current task does not match expected task ID');
        return;
      }
    }
    
    ASYNC_CURRENT_TASK_IDS.pop();
    ASYNC_CURRENT_COMPONENT_IDXS.pop();
    
    const taskMeta = tasks.pop();
    return taskMeta.task;
  }
  
  const CURRENT_TASK_MAY_BLOCK= globalThis.WebAssembly ? new globalThis.WebAssembly.Global({ value: 'i32', mutable: true }, 0) : false;
  
  const ASYNC_CURRENT_TASK_IDS = [];
  const ASYNC_CURRENT_COMPONENT_IDXS = [];
  
  function unpackCallbackResult(result) {
    if (!(_typeCheckValidI32(result))) { throw new Error('invalid callback return value [' + result + '], not a valid i32'); }
    const eventCode = result & 0xF;
    if (eventCode < 0 || eventCode > 3) {
      throw new Error('invalid async return value [' + eventCode + '], outside callback code range');
    }
    if (result < 0 || result >= 2**32) { throw new Error('invalid callback result'); }
    // TODO: table max length check?
    const waitableSetRep = result >> 4;
    return [eventCode, waitableSetRep];
  }
  
  class AsyncSubtask {
    static _ID = 0n;
    
    static State = {
      STARTING: 0,
      STARTED: 1,
      RETURNED: 2,
      CANCELLED_BEFORE_STARTED: 3,
      CANCELLED_BEFORE_RETURNED: 4,
    };
    
    #id;
    #state = AsyncSubtask.State.STARTING;
    #componentIdx;
    
    #parentTask;
    #childTask = null;
    
    #dropped = false;
    #cancelRequested = false;
    
    #memoryIdx = null;
    #lenders = null;
    
    #waitable = null;
    
    #callbackFn = null;
    #callbackFnName = null;
    
    #postReturnFn = null;
    #onProgressFn = null;
    #pendingEventFn = null;
    
    #callMetadata = {};
    
    #resolved = false;
    
    #onResolveHandlers = [];
    #onStartHandlers = [];
    
    #result = null;
    #resultSet = false;
    
    fnName;
    target;
    isAsync;
    isManualAsync;
    
    constructor(args) {
      if (typeof args.componentIdx !== 'number') {
        throw new Error('invalid componentIdx for subtask creation');
      }
      this.#componentIdx = args.componentIdx;
      
      this.#id = ++AsyncSubtask._ID;
      this.fnName = args.fnName;
      
      if (!args.parentTask) { throw new Error('missing parent task during subtask creation'); }
      this.#parentTask = args.parentTask;
      
      if (args.childTask) { this.#childTask = args.childTask; }
      
      if (args.memoryIdx) { this.#memoryIdx = args.memoryIdx; }
      
      if (!args.waitable) { throw new Error("missing/invalid waitable"); }
      this.#waitable = args.waitable;
      
      if (args.callMetadata) { this.#callMetadata = args.callMetadata; }
      
      this.#lenders = [];
      this.target = args.target;
      this.isAsync = args.isAsync;
      this.isManualAsync = args.isManualAsync;
    }
    
    id() { return this.#id; }
    parentTaskID() { return this.#parentTask?.id(); }
    childTaskID() { return this.#childTask?.id(); }
    state() { return this.#state; }
    
    waitable() { return this.#waitable; }
    waitableRep() { return this.#waitable.idx(); }
    
    join() { return this.#waitable.join(...arguments); }
    getPendingEvent() { return this.#waitable.getPendingEvent(...arguments); }
    hasPendingEvent() { return this.#waitable.hasPendingEvent(...arguments); }
    setPendingEvent() { return this.#waitable.setPendingEvent(...arguments); }
    
    setTarget(tgt) { this.target = tgt; }
    
    getResult() {
      if (!this.#resultSet) { throw new Error("subtask result has not been set") }
      return this.#result;
    }
    setResult(v) {
      if (this.#resultSet) { throw new Error("subtask result has already been set"); }
      this.#result = v;
      this.#resultSet = true;
    }
    
    componentIdx() { return this.#componentIdx; }
    
    setChildTask(t) {
      if (!t) { throw new Error('cannot set missing/invalid child task on subtask'); }
      if (this.#childTask) { throw new Error('child task is already set on subtask'); }
      if (this.#parentTask === t) { throw new Error("parent cannot be child"); }
      this.#childTask = t;
    }
    getChildTask(t) { return this.#childTask; }
    
    getParentTask() { return this.#parentTask; }
    
    setCallbackFn(f, name) {
      if (!f) { return; }
      if (this.#callbackFn) { throw new Error('callback fn can only be set once'); }
      this.#callbackFn = f;
      this.#callbackFnName = name;
    }
    
    getCallbackFnName() {
      if (!this.#callbackFn) { return undefined; }
      return this.#callbackFn.name;
    }
    
    setPostReturnFn(f) {
      if (!f) { return; }
      if (this.#postReturnFn) { throw new Error('postReturn fn can only be set once'); }
      this.#postReturnFn = f;
    }
    
    setOnProgressFn(f) {
      if (this.#onProgressFn) { throw new Error('on progress fn can only be set once'); }
      this.#onProgressFn = f;
    }
    
    isNotStarted() {
      return this.#state == AsyncSubtask.State.STARTING;
    }
    
    registerOnStartHandler(f) {
      this.#onStartHandlers.push(f);
    }
    
    onStart(args) {
      _debugLog('[AsyncSubtask#onStart()] args', {
        componentIdx: this.#componentIdx,
        subtaskID: this.#id,
        parentTaskID: this.parentTaskID(),
        fnName: this.fnName,
      });
      
      if (this.#onProgressFn) { this.#onProgressFn(); }
      
      this.#state = AsyncSubtask.State.STARTED;
      
      let result;
      
      // If we have been provided a helper start function as a result of
      // component fusion performed by wasmtime tooling, then we can call that helper and lifts/lowers will
      // be performed for us.
      //
      // See also documentation on `HostIntrinsic::PrepareCall`
      //
      if (this.#callMetadata.startFn) {
        result = this.#callMetadata.startFn.apply(null, args?.startFnParams ?? []);
      }
      
      return result;
    }
    
    
    registerOnResolveHandler(f) {
      this.#onResolveHandlers.push(f);
    }
    
    reject(subtaskErr) {
      this.#childTask?.reject(subtaskErr);
    }
    
    onResolve(subtaskValue) {
      _debugLog('[AsyncSubtask#onResolve()] args', {
        componentIdx: this.#componentIdx,
        subtaskID: this.#id,
        isAsync: this.isAsync,
        childTaskID: this.childTaskID(),
        parentTaskID: this.parentTaskID(),
        parentTaskFnName: this.#parentTask?.entryFnName(),
        fnName: this.fnName,
      });
      
      if (this.#resolved) {
        throw new Error('subtask has already been resolved');
      }
      
      if (this.#onProgressFn) { this.#onProgressFn(); }
      
      if (subtaskValue === null && this.#cancelRequested) {
        if (this.#state === AsyncSubtask.State.STARTING) {
          this.#state = AsyncSubtask.State.CANCELLED_BEFORE_STARTED;
        } else {
          if (this.#state !== AsyncSubtask.State.STARTED) {
            throw new Error('resolved subtask must have been started before cancellation');
          }
          this.#state = AsyncSubtask.State.CANCELLED_BEFORE_RETURNED;
        }
      } else {
        if (this.#state !== AsyncSubtask.State.STARTED) {
          throw new Error('resolved subtask must have been started before completion');
        }
        this.#state = AsyncSubtask.State.RETURNED;
      }
      
      this.setResult(subtaskValue);
      
      for (const f of this.#onResolveHandlers) {
        try {
          f(subtaskValue);
        } catch (err) {
          console.error("error during subtask resolve handler", err);
          throw err;
        }
      }
      
      const callMetadata = this.getCallMetadata();
      
      // TODO(fix): we should be able to easily have the caller's meomry
      // to lower into here, but it's not present in PrepareCall
      const memory = callMetadata.memory ?? this.#parentTask?.getReturnMemory() ?? lookupMemoriesForComponent({ componentIdx: this.#parentTask?.componentIdx() })[0];
      if (callMetadata && !callMetadata.returnFn && this.isAsync && callMetadata.resultPtr && memory) {
        const { resultPtr, realloc } = callMetadata;
        const lowers = callMetadata.lowers; // may have been updated in task.return of the child
        if (lowers && lowers.length > 0) {
          lowers[0]({
            componentIdx: this.#componentIdx,
            memory,
            realloc,
            vals: [subtaskValue],
            storagePtr: resultPtr,
            stringEncoding: callMetadata.stringEncoding,
          });
        }
      }
      
      this.#resolved = true;
      this.#parentTask.removeSubtask(this);
    }
    
    getStateNumber() { return this.#state; }
    isReturned() { return this.#state === AsyncSubtask.State.RETURNED; }
    
    getCallMetadata() { return this.#callMetadata; }
    
    isResolved() {
      if (this.#state === AsyncSubtask.State.STARTING
      || this.#state === AsyncSubtask.State.STARTED) {
        return false;
      }
      if (this.#state === AsyncSubtask.State.RETURNED
      || this.#state === AsyncSubtask.State.CANCELLED_BEFORE_STARTED
      || this.#state === AsyncSubtask.State.CANCELLED_BEFORE_RETURNED) {
        return true;
      }
      throw new Error('unrecognized internal Subtask state [' + this.#state + ']');
    }
    
    addLender(handle) {
      _debugLog('[AsyncSubtask#addLender()] args', { handle });
      if (!Number.isNumber(handle)) { throw new Error('missing/invalid lender handle [' + handle + ']'); }
      
      if (this.#lenders.length === 0 || this.isResolved()) {
        throw new Error('subtask has no lendors or has already been resolved');
      }
      
      handle.lends++;
      this.#lenders.push(handle);
    }
    
    deliverResolve() {
      _debugLog('[AsyncSubtask#deliverResolve()] args', {
        lenders: this.#lenders,
        parentTaskID: this.parentTaskID(),
        subtaskID: this.#id,
        childTaskID: this.childTaskID(),
        resolved: this.isResolved(),
        resolveDelivered: this.resolveDelivered(),
      });
      
      const cannotDeliverResolve = this.resolveDelivered() || !this.isResolved();
      if (cannotDeliverResolve) {
        throw new Error('subtask cannot deliver resolution twice, and the subtask must be resolved');
      }
      
      for (const lender of this.#lenders) {
        lender.lends--;
      }
      
      this.#lenders = null;
    }
    
    resolveDelivered() {
      _debugLog('[AsyncSubtask#resolveDelivered()] args', { });
      if (this.#lenders === null && !this.isResolved()) {
        throw new Error('invalid subtask state, lenders missing and subtask has not been resolved');
      }
      return this.#lenders === null;
    }
    
    drop() {
      _debugLog('[AsyncSubtask#drop()] args', {
        componentIdx: this.#componentIdx,
        parentTaskID: this.#parentTask?.id(),
        parentTaskFnName: this.#parentTask?.entryFnName(),
        childTaskID: this.#childTask?.id(),
        childTaskFnName: this.#childTask?.entryFnName(),
        subtaskFnName: this.fnName,
      });
      if (!this.#waitable) { throw new Error('missing/invalid inner waitable'); }
      if (!this.resolveDelivered()) {
        throw new Error('cannot drop subtask before resolve is delivered');
      }
      if (this.#waitable) { this.#waitable.drop() }
      this.#dropped = true;
    }
    
    #getComponentState() {
      const state = getOrCreateAsyncState(this.#componentIdx);
      if (!state) {
        throw new Error('invalid/missing async state for component [' + componentIdx + ']');
      }
      return state;
    }
    
    getWaitableHandleIdx() {
      _debugLog('[AsyncSubtask#getWaitableHandleIdx()] args', { });
      if (!this.#waitable) { throw new Error('missing/invalid waitable'); }
      return this.waitableRep();
    }
  }
  
  function _prepareCall(
  memoryIdx,
  getMemoryFn,
  startFn,
  returnFn,
  callerComponentIdx,
  calleeComponentIdx,
  taskReturnTypeIdx,
  calleeIsAsyncInt,
  stringEncoding,
  resultCountOrAsync,
  ) {
    _debugLog('[_prepareCall()]', {
      memoryIdx,
      callerComponentIdx,
      calleeComponentIdx,
      taskReturnTypeIdx,
      calleeIsAsyncInt,
      stringEncoding,
      resultCountOrAsync,
    });
    const argArray = [...arguments];
    
    // value passed in *may* be as large as u32::MAX which may be mangled into -2
    resultCountOrAsync >>>= 0;
    
    let isAsync = false;
    let hasResultPointer = false;
    if (resultCountOrAsync === 2**32 - 1) {
      // prepare async with no result (u32::MAX)
      isAsync = true;
      hasResultPointer = false;
    } else if (resultCountOrAsync === 2**32 - 2) {
      // prepare async with result (u32::MAX - 1)
      isAsync = true;
      hasResultPointer = true;
    }
    
    const currentCallerTaskMeta = getCurrentTask(callerComponentIdx);
    if (!currentCallerTaskMeta) {
      throw new Error('invalid/missing current task for caller during prepare call');
    }
    
    const currentCallerTask = currentCallerTaskMeta.task;
    if (!currentCallerTask) {
      throw new Error('unexpectedly missing task in meta for caller during prepare call');
    }
    
    if (currentCallerTask.componentIdx() !== callerComponentIdx) {
      throw new Error(`task component idx [${ currentCallerTask.componentIdx() }] !== [${ callerComponentIdx }] (callee ${ calleeComponentIdx })`);
    }
    
    let getCalleeParamsFn;
    let resultPtr = null;
    let directParamsArr;
    if (hasResultPointer) {
      directParamsArr = argArray.slice(10, argArray.length - 1);
      getCalleeParamsFn = () => directParamsArr;
      resultPtr = argArray[argArray.length - 1];
    } else {
      directParamsArr = argArray.slice(10);
      getCalleeParamsFn = () => directParamsArr;
    }
    
    let encoding;
    switch (stringEncoding) {
      case 0:
      encoding = 'utf8';
      break;
      case 1:
      encoding = 'utf16';
      break;
      case 2:
      encoding = 'compact-utf16';
      break;
      default:
      throw new Error(`unrecognized string encoding enum [${stringEncoding}]`);
    }
    
    const subtask = currentCallerTask.createSubtask({
      componentIdx: callerComponentIdx,
      parentTask: currentCallerTask,
      isAsync,
      callMetadata: {
        getMemoryFn,
        memoryIdx,
        resultPtr,
        returnFn,
        startFn,
        stringEncoding,
      }
    });
    
    const [newTask, newTaskID] = createNewCurrentTask({
      componentIdx: calleeComponentIdx,
      isAsync,
      getCalleeParamsFn,
      entryFnName: [
      'task',
      subtask.getParentTask().id(),
      'subtask',
      subtask.id(),
      'new-prepared-async-task'
      ].join('/'),
      stringEncoding,
    });
    newTask.setParentSubtask(subtask);
    newTask.setReturnMemoryIdx(memoryIdx);
    newTask.setReturnMemory(getMemoryFn);
    subtask.setChildTask(newTask);
    
    newTask.subtaskMeta = {
      subtask,
      calleeComponentIdx,
      callerComponentIdx,
      getCalleeParamsFn,
      stringEncoding,
      isAsync,
    };
    
    _setGlobalCurrentTaskMeta({
      taskID: newTask.id(),
      componentIdx: newTask.componentIdx(),
    });
  }
  
  function _asyncStartCall(args, callee, paramCount, resultCount, flags) {
    const componentIdx = ASYNC_CURRENT_COMPONENT_IDXS.at(-1);
    
    const globalTaskMeta = _getGlobalCurrentTaskMeta(componentIdx);
    if (!globalTaskMeta) { throw new Error('missing global current task globalTaskMeta'); }
    const taskID = globalTaskMeta.taskID;
    
    _debugLog('[_asyncStartCall()] args', { args, componentIdx });
    const { getCallbackFn, callbackIdx, getPostReturnFn, postReturnIdx } = args;
    
    const preparedTaskMeta = getCurrentTask(componentIdx, taskID);
    if (!preparedTaskMeta) { throw new Error('unexpectedly missing current task'); }
    
    const preparedTask = preparedTaskMeta.task;
    if (!preparedTask) { throw new Error('unexpectedly missing current task'); }
    if (!preparedTask.subtaskMeta) { throw new Error('missing subtask meta from prepare'); }
    
    const {
      subtask,
      returnMemoryIdx,
      getReturnMemoryFn,
      callerComponentIdx,
      calleeComponentIdx,
      getCalleeParamsFn,
      isAsync,
      stringEncoding,
    } = preparedTask.subtaskMeta;
    if (!subtask) { throw new Error("missing subtask from cstate during async start call"); }
    if (calleeComponentIdx !== preparedTask.componentIdx()) {
      throw new Error(`meta callee idx [${calleeComponentIdx}] != current task idx [${preparedTask.componentIdx()}] during async start call`);
    }
    if (calleeComponentIdx !== componentIdx) {
      throw new Error("mismatched componentIdx for async start call (does not match prepare)");
    }
    
    const argArray = [...arguments];
    
    if (resultCount < 0 || resultCount > 1) { throw new Error('invalid/unsupported result count'); }
    
    const callbackFnName = 'callback_' + callbackIdx;
    const callbackFn = getCallbackFn();
    preparedTask.setCallbackFn(callbackFn, callbackFnName);
    preparedTask.setPostReturnFn(getPostReturnFn());
    
    if (resultCount < 0 || resultCount > 1) {
      throw new Error(`unsupported result count [${ resultCount }]`);
    }
    
    const params = preparedTask.getCalleeParams();
    if (paramCount !== params.length) {
      throw new Error(`unexpected callee param count [${ params.length }], _asyncStartCall invocation expected [${ paramCount }]`);
    }
    
    const callerComponentState = getOrCreateAsyncState(subtask.componentIdx());
    
    const calleeComponentState = getOrCreateAsyncState(preparedTask.componentIdx());
    const calleeBackpressure = calleeComponentState.hasBackpressure();
    
    // Set up a handler on subtask completion to lower results from the call into the caller's memory region.
    //
    // NOTE: during fused guest->guest calls this handler is triggered, but does not actually perform
    // lowering manually, as fused modules provider helper functions that can
    subtask.registerOnResolveHandler((res) => {
      _debugLog('[_asyncStartCall()] handling subtask result', { res, subtaskID: subtask.id() });
      
      let subtaskCallMeta = subtask.getCallMetadata();
      
      // NOTE: in the case of guest -> guest async calls, there may be no memory/realloc present,
      // as the host will intermediate the value storage/movement between calls.
      //
      // We can simply take the value and lower it as a parameter
      if (subtaskCallMeta.memory || subtaskCallMeta.realloc) {
        throw new Error("call metadata unexpectedly contains memory/realloc for guest->guest call");
      }
      
      const callerTask = subtask.getParentTask();
      const calleeTask = preparedTask;
      const callerMemoryIdx = callerTask.getReturnMemoryIdx();
      const callerComponentIdx = callerTask.componentIdx();
      
      // If a helper function was provided we are likely in a fused guest->guest call,
      // and the result will be delivered (lift/lowered) via helper function
      if (subtaskCallMeta && subtaskCallMeta.returnFn) {
        _debugLog('[_asyncStartCall()] return function present while handling subtask result, returning early (skipping lower)');
        
        // TODO: centralize calling of returnFn to *one place* (if possible)
        if (subtaskCallMeta.returnFnCalled) { return; }
        
        subtaskCallMeta.returnFn.apply(null, [subtaskCallMeta.resultPtr]);
        return;
      }
      
      // If there is no where to lower the results, exit early
      if (!subtaskCallMeta.resultPtr) {
        _debugLog('[_asyncStartCall()] no result ptr during subtask result handling, returning early (skipping lower)');
        return;
      }
      
      let callerMemory;
      if (callerMemoryIdx !== null && callerMemoryIdx !== undefined) {
        callerMemory = lookupMemoriesForComponent({ componentIdx: callerComponentIdx, memoryIdx: callerMemoryIdx });
      } else {
        const callerMemories = lookupMemoriesForComponent({ componentIdx: callerComponentIdx });
        if (callerMemories.length !== 1) { throw new Error(`unsupported amount of caller memories`); }
        callerMemory = callerMemories[0];
      }
      
      if (!callerMemory) {
        _debugLog('[_asyncStartCall()] missing memory', { subtaskID: subtask.id(), res });
        throw new Error(`missing memory for to guest->guest call result (subtask [${subtask.id()}])`);
      }
      
      const lowerFns = calleeTask.getReturnLowerFns();
      if (!lowerFns || lowerFns.length === 0) {
        _debugLog('[_asyncStartCall()] missing result lower metadata for guest->guest call', { subtaskID: subtask.id() });
        throw new Error(`missing result lower metadata for guest->guest call (subtask [${subtask.id()}])`);
      }
      
      if (lowerFns.length !== 1) {
        _debugLog('[_asyncStartCall()] only single result reportetd for guest->guest call', { subtaskID: subtask.id() });
        throw new Error(`only single result supported for guest->guest calls (subtask [${subtask.id()}])`);
      }
      
      _debugLog('[_asyncStartCall()] lowering results', { subtaskID: subtask.id() });
      lowerFns[0]({
        realloc: undefined,
        memory: callerMemory,
        vals: [res],
        storagePtr: subtaskCallMeta.resultPtr,
        componentIdx: callerComponentIdx,
        stringEncoding: subtaskCallMeta.stringEncoding,
      });
      
    });
    
    subtask.setOnProgressFn(() => {
      subtask.setPendingEvent(() => {
        if (subtask.isResolved()) { subtask.deliverResolve(); }
        const event = {
          code: ASYNC_EVENT_CODE.SUBTASK,
          payload0: subtask.waitableRep(),
          payload1: subtask.getStateNumber(),
        };
        return event;
      });
    });
    
    // Start the (event) driver loop that will resolve the task
    queueMicrotask(async () => {
      let startRes = subtask.onStart({ startFnParams: params });
      startRes = Array.isArray(startRes) ? startRes : [startRes];
      
      await calleeComponentState.suspendTask({
        task: preparedTask,
        readyFn: () => !calleeComponentState.isExclusivelyLocked(),
      });
      
      const started = await preparedTask.enter();
      if (!started) {
        _debugLog('[_asyncStartCall()] task failed early', {
          taskID: preparedTask.id(),
          subtaskID: subtask.id(),
        });
        throw new Error("task failed to start");
        return;
      }
      
      let callbackResult;
      try {
        let jspiCallee = WebAssembly.promising(callee);
        callbackResult = await _withGlobalCurrentTaskMetaAsync({
          taskID: preparedTask.id(),
          componentIdx: preparedTask.componentIdx(),
          fn: () => {
            return jspiCallee.apply(null, startRes);
          }
        });
      } catch(err) {
        _debugLog("[_asyncStartCall()] initial subtask callee run failed", err);
        // NOTE: a good place to rejectt the parent task, if rejection API is enabled
        // subtask.reject(err);
        // subtask.getParentTask().reject(err);
        
        subtask.getParentTask().setErrored(err);
        
        return;
      }
      
      // If there was no callback function, we're dealing with a sync function
      // that was lifted as async without one, there is only the callee.
      if (!callbackFn) {
        _debugLog("[_asyncStartCall()] no callback, resolving w/ callee result", {
          taskID: preparedTask.id(),
          componentIdx: preparedTask.componentIdx(),
          preparedTask,
          stateNumber: preparedTask.taskState(),
          isResolved: preparedTask.isResolved(),
          callbackFn,
        });
        preparedTask.resolve([callbackResult]);
        return;
      }
      
      let fnName = callbackFn.fnName;
      if (!fnName) {
        fnName = [
        '<task ',
        subtask.parentTaskID(),
        '/subtask ',
        subtask.id(),
        '/task ',
        preparedTask.id(),
        '>',
        ].join("");
      }
      
      try {
        _debugLog("[_asyncStartCall()] starting driver loop", {
          fnName,
          componentIdx: preparedTask.componentIdx(),
          subtaskID: subtask.id(),
          childTaskID: subtask.childTaskID(),
          parentTaskID: subtask.parentTaskID(),
        });
        
        await _driverLoop({
          componentState: calleeComponentState,
          task: preparedTask,
          fnName,
          isAsync: true,
          callbackResult,
          resolve,
          reject
        });
      } catch (err) {
        _debugLog("[AsyncStartCall] drive loop call failure", { err });
      }
      
    });
    
    const subtaskState = subtask.getStateNumber();
    if (subtaskState < 0 || subtaskState > 2**5) {
      throw new Error('invalid subtask state, out of valid range');
    }
    
    _debugLog('[_asyncStartCall()] returning subtask rep & state', {
      subtask: {
        rep: subtask.waitableRep(),
        state: subtaskState,
      }
    });
    
    return Number(subtask.waitableRep()) << 4 | subtaskState;
  }
  
  function _syncStartCall(callbackIdx) {
    _debugLog('[_syncStartCall()] args', { callbackIdx });
    throw new Error('synchronous start call not implemented!');
  }
  
  class Waitable {
    #componentIdx;
    
    #pendingEventFn = null;
    
    #promise;
    #resolve;
    #reject;
    
    #waitableSet = null;
    
    #idx = null; // to component-global waitables
    
    target;
    
    constructor(args) {
      const { componentIdx, target } = args;
      this.#componentIdx = componentIdx;
      this.target = args.target;
      this.#resetPromise();
    }
    
    componentIdx() { return this.#componentIdx; }
    isInSet() { return this.#waitableSet !== null; }
    
    idx() { return this.#idx; }
    setIdx(idx) {
      if (idx === 0) { throw new Error("waitable idx cannot be zero"); }
      this.#idx = idx;
    }
    
    setTarget(tgt) { this.target = tgt; }
    
    #resetPromise() {
      const { promise, resolve, reject } = promiseWithResolvers()
      this.#promise = promise;
      this.#resolve = resolve;
      this.#reject = reject;
    }
    
    resolve() { this.#resolve(); }
    reject(err) { this.#reject(err); }
    promise() { return this.#promise; }
    
    hasPendingEvent() {
      // _debugLog('[Waitable#hasPendingEvent()]', {
        //     componentIdx: this.#componentIdx,
        //     waitable: this,
        //     waitableSet: this.#waitableSet,
        //     hasPendingEvent: this.#pendingEventFn !== null,
        // });
        return this.#pendingEventFn !== null;
      }
      
      setPendingEvent(fn) {
        _debugLog('[Waitable#setPendingEvent()] args', {
          waitable: this,
          inSet: this.#waitableSet,
        });
        this.#pendingEventFn = fn;
      }
      
      getPendingEvent() {
        _debugLog('[Waitable#getPendingEvent()] args', {
          waitable: this,
          inSet: this.#waitableSet,
          hasPendingEvent: this.#pendingEventFn !== null,
        });
        if (this.#pendingEventFn === null) { return null; }
        const eventFn = this.#pendingEventFn;
        this.#pendingEventFn = null;
        const e = eventFn();
        this.#resetPromise();
        return e;
      }
      
      join(waitableSet) {
        _debugLog('[Waitable#join()] args', {
          waitable: this,
          waitableSet: waitableSet,
        });
        if (this.#waitableSet) { this.#waitableSet.removeWaitable(this); }
        if (!waitableSet) {
          this.#waitableSet = null;
          return;
        }
        waitableSet.addWaitable(this);
        this.#waitableSet = waitableSet;
      }
      
      drop() {
        _debugLog('[Waitable#drop()] args', {
          componentIdx: this.#componentIdx,
          waitable: this,
        });
        if (this.hasPendingEvent()) {
          throw new Error('waitables with pending events cannot be dropped');
        }
        this.join(null);
      }
      
    }
    
    const ERR_CTX_TABLES = {};
    
    const emptyFunc = () => {};
    
    let dv = new DataView(new ArrayBuffer());
    const dataView = mem => dv.buffer === mem.buffer ? dv : dv = new DataView(mem.buffer);
    
    function toUint64(val) {
      const converted = BigInt(val)
      
      return BigInt.asUintN(64, converted);
    }
    
    
    function toInt32(val) {
      
      return val >> 0;
    }
    
    const utf16Decoder = new TextDecoder('utf-16');
    const TEXT_DECODER_UTF8 = new TextDecoder();
    const TEXT_ENCODER_UTF8 = new TextEncoder();
    
    async function _utf8AllocateAndEncodeAsync(s, realloc, memory) {
      if (typeof s !== 'string') {
        throw new TypeError('expected a string, received [' + typeof s + ']');
      }
      if (s.length === 0) { return { ptr: 1, len: 0 }; }
      let buf = TEXT_ENCODER_UTF8.encode(s);
      let ptr = await realloc(0, 0, 1, buf.length);
      new Uint8Array(memory.buffer).set(buf, ptr);
      const res = { ptr, len: buf.length, codepoints: [...s].length };
      return res;
    }
    
    
    const T_FLAG = 1 << 30;
    
    function rscTableCreateOwn(table, rep) {
      const free = table[0] & ~T_FLAG;
      if (free === 0) {
        table.push(0);
        table.push(rep | T_FLAG);
        return (table.length >> 1) - 1;
      }
      table[0] = table[free << 1];
      table[free << 1] = 0;
      table[(free << 1) + 1] = rep | T_FLAG;
      return free;
    }
    
    function rscTableRemove(table, handle) {
      const scope = table[handle << 1];
      const val = table[(handle << 1) + 1];
      const own = (val & T_FLAG) !== 0;
      const rep = val & ~T_FLAG;
      if (val === 0 || (scope & T_FLAG) !== 0) {
        throw new TypeError("Invalid handle");
      }
      table[handle << 1] = table[0] | T_FLAG;
      table[0] = handle | T_FLAG;
      return { rep, scope, own };
    }
    
    let curResourceBorrows = [];
    
    function contextSet(ctx, value) {
      const { componentIdx, slot } = ctx;
      if (componentIdx === undefined) { throw new TypeError("missing component idx"); }
      if (slot === undefined) { throw new TypeError("missing slot"); }
      if (!(_typeCheckValidI32(value))) { throw new Error('invalid value for context set (not valid i32)'); }
      
      const currentTaskMeta = _getGlobalCurrentTaskMeta(componentIdx);
      if (!currentTaskMeta) {
        throw new Error(`missing/incomplete global current task meta for component idx [${componentIdx}] during context set`);
      }
      const taskID = currentTaskMeta.taskID;
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) { throw new Error('failed to retrieve current task'); }
      
      let task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing current task in metadata while setting context'); }
      
      _debugLog('[contextSet()] args', {
        slot,
        value,
        storage: task.storage,
        taskID: task.id(),
        componentIdx: task.componentIdx(),
      });
      
      if (slot < 0 || slot >= task.storage.length) { throw new Error('invalid slot for current task'); }
      task.storage[slot] = value;
    }
    
    
    function contextGet(ctx) {
      const { componentIdx, slot } = ctx;
      if (componentIdx === undefined) { throw new TypeError("missing component idx"); }
      if (slot === undefined) { throw new TypeError("missing slot"); }
      
      const currentTaskMeta = _getGlobalCurrentTaskMeta(componentIdx);
      if (!currentTaskMeta) {
        throw new Error(`missing/incomplete global current task meta for component idx [${componentIdx}] during context set`);
      }
      const taskID = currentTaskMeta.taskID;
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) { throw new Error('failed to retrieve current task'); }
      
      let task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing current task in metadata while getting context'); }
      
      _debugLog('[contextGet()] args', {
        slot,
        storage: task.storage,
        taskID: task.id(),
        componentIdx: task.componentIdx(),
      });
      
      if (slot < 0 || slot >= task.storage.length) { throw new Error('invalid slot for current task'); }
      
      return task.storage[slot];
    }
    
    
    function taskReturn(ctx) {
      const {
        componentIdx,
        getMemoryFn,
        memoryIdx,
        callbackFnIdx,
        liftFns,
        lowerFns,
        stringEncoding,
      } = ctx;
      const params = [...arguments].slice(1);
      const memory = getMemoryFn();
      let useDirectParams = ctx.useDirectParams;
      
      const { taskID } = _getGlobalCurrentTaskMeta(componentIdx);
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) { throw new Error('failed to retrieve current task metadata'); }
      
      const task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing current task in metadata'); }
      
      _debugLog('[taskReturn()] args', {
        componentIdx,
        taskID: task.id(),
        subtaskID: task.getParentSubtask()?.id(),
        callbackFnIdx,
        memoryIdx,
        liftFns,
        lowerFns,
        params,
      });
      
      // If we are in a subtask, and have a fused helper function provided to use
      // via PrepareCall, we can use that function rather than performing lifting manually.
      //
      // See also documentation on `HostIntrinsic::PrepareCall`
      const subtaskCallMetadata = task.getParentSubtask()?.getCallMetadata();
      if (subtaskCallMetadata?.returnFn) {
        subtaskCallMetadata.returnFn.apply(null, [...params, subtaskCallMetadata.resultPtr]);
        subtaskCallMetadata.returnFnCalled = true;
        task.resolve([]);
        return;
      }
      
      const expectedMemoryIdx = task.getReturnMemoryIdx();
      if (expectedMemoryIdx !== null && memoryIdx !== null && expectedMemoryIdx !== memoryIdx) {
        _debugLog("[taskReturn()] mismatched memory indices", { expectedMemoryIdx, memoryIdx });
        throw new Error('task.return memory [' + memoryIdx + '] does not match task [' + expectedMemoryIdx + ']');
      }
      
      task.callbackFnIdx = callbackFnIdx;
      
      if (!memory && liftFns.length > 4) {
        _debugLog("[taskReturn()] memory not present for max async flat lifts");
        throw new Error('memory must be present if more than max async flat lifts are performed');
      }
      
      let liftCtx = { memory, useDirectParams, params, componentIdx, stringEncoding };
      if (!useDirectParams) {
        if (!ctx.memory) {
          _debugLog('missing memory despite indirect param usage', { useDirectParams, liftCtx, ctx });
          throw new Error('missing memory despite indirect param usage');
        }
        liftCtx.storagePtr = params[0];
        liftCtx.storageLen = params[1];
      }
      
      const liftedResults = [];
      _debugLog('[taskReturn()] lifting results out of memory', { liftCtx });
      for (const liftFn of liftFns) {
        if (liftCtx.storageLen !== undefined && liftCtx.storageLen <= 0) {
          _debugLog(`[taskReturn()] ran out of range while writing storageLen = [${liftCtx.storageLen}]`);
          throw new Error('ran out of storage while writing');
        }
        const [ val, newLiftCtx ] = liftFn(liftCtx);
        liftCtx = newLiftCtx;
        liftedResults.push(val);
      }
      
      task.resolve(liftedResults);
    }
    
    function subtaskDrop(componentIdx, subtaskWaitableRep) {
      _debugLog('[subtaskDrop()] args', { componentIdx, subtaskWaitableRep });
      
      const cstate = getOrCreateAsyncState(componentIdx);
      if (!cstate.mayLeave) { throw new Error('component is not marked as may leave, cannot be cancelled'); }
      
      const subtask =  cstate.handles.remove(subtaskWaitableRep);
      if (!subtask) { throw new Error('missing/invalid subtask specified for drop in component instance'); }
      
      subtask.drop();
    }
    
    function subtaskCancel(componentIdx, isAsync) {
      _debugLog('[subtaskCancel()] args', { componentIdx, isAsync });
      
      const state = getOrCreateAsyncState(componentIdx);
      if (!state.mayLeave) { throw new Error('component instance is not marked as may leave, cannot be cancelled'); }
      
      const { taskID } = _getGlobalCurrentTaskMeta(componentIdx);
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) { throw new Error('invalid/missing async task meta'); }
      
      const task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing async task'); }
      
      if (task.sync && !task.alwaysTaskReturn) {
        throw new Error('cannot cancel sync tasks without always task return set');
      }
      if (!task.cancelRequested) { throw new Error('task cancellation has not been requested'); }
      if (task.borrowedHandles.length > 0) { throw new Error('task still has borrow handles'); }
      if (task.returnCalls > 0) { throw new Error('cannot cancel task that has already returned a value'); }
      if (task.cancelled) { throw new Error('cannot cancel task that has already been cancelled'); }
      
      task.cancelled = true;
    }
    
    function taskCancel(componentIdx) {
      _debugLog('[taskCancel()] args', { componentIdx, isAsync });
      
      const state = getOrCreateAsyncState(componentIdx);
      if (!state.mayLeave) { throw new Error('component instance is not marked as may leave, cannot be cancelled'); }
      
      const { taskID } = _getGlobalCurrentTaskMeta(componentIdx);
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) { throw new Error('invalid/missing async task meta'); }
      
      const task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing async task'); }
      
      if (task.sync && !task.alwaysTaskReturn) {
        throw new Error('cannot cancel sync tasks without always task return set');
      }
      
      task.cancel();
    }
    
    function getCurrentTask(componentIdx, taskID) {
      let usedGlobal = false;
      if (componentIdx === undefined || componentIdx === null) {
        throw new Error('missing component idx'); // TODO(fix)
        // componentIdx = ASYNC_CURRENT_COMPONENT_IDXS.at(-1);
        // usedGlobal = true;
      }
      
      const taskMetas = ASYNC_TASKS_BY_COMPONENT_IDX.get(componentIdx);
      if (taskMetas === undefined || taskMetas.length === 0) { return undefined; }
      
      if (taskID) {
        return taskMetas.find(meta => meta.task.id() === taskID);
      }
      
      const taskMeta = taskMetas[taskMetas.length - 1];
      if (!taskMeta || !taskMeta.task) { return undefined; }
      
      return taskMeta;
    }
    
    function createNewCurrentTask(args) {
      _debugLog('[createNewCurrentTask()] args', args);
      const {
        componentIdx,
        isAsync,
        isManualAsync,
        entryFnName,
        parentSubtaskID,
        callbackFnName,
        getCallbackFn,
        getParamsFn,
        stringEncoding,
        errHandling,
        getCalleeParamsFn,
        resultPtr,
        callingWasmExport,
      } = args;
      if (componentIdx === undefined || componentIdx === null) {
        throw new Error('missing/invalid component instance index while starting task');
      }
      let taskMetas = ASYNC_TASKS_BY_COMPONENT_IDX.get(componentIdx);
      const callbackFn = getCallbackFn ? getCallbackFn() : null;
      
      const newTask = new AsyncTask({
        componentIdx,
        isAsync,
        isManualAsync,
        entryFnName,
        callbackFn,
        callbackFnName,
        stringEncoding,
        getCalleeParamsFn,
        resultPtr,
        errHandling,
      });
      
      const newTaskID = newTask.id();
      const newTaskMeta = { id: newTaskID, componentIdx, task: newTask };
      
      // NOTE: do not track host tasks
      ASYNC_CURRENT_TASK_IDS.push(newTaskID);
      ASYNC_CURRENT_COMPONENT_IDXS.push(componentIdx);
      
      if (!taskMetas) {
        taskMetas = [newTaskMeta];
        ASYNC_TASKS_BY_COMPONENT_IDX.set(componentIdx, [newTaskMeta]);
      } else {
        taskMetas.push(newTaskMeta);
      }
      
      return [newTask, newTaskID];
    }
    const ASYNC_TASKS_BY_COMPONENT_IDX = new Map();
    
    class AsyncTask {
      static _ID = 0n;
      
      static State = {
        INITIAL: 'initial',
        CANCELLED: 'cancelled',
        CANCEL_PENDING: 'cancel-pending',
        CANCEL_DELIVERED: 'cancel-delivered',
        RESOLVED: 'resolved',
      }
      
      static BlockResult = {
        CANCELLED: 'block.cancelled',
        NOT_CANCELLED: 'block.not-cancelled',
      }
      
      #id;
      #componentIdx;
      #state;
      #isAsync;
      #isManualAsync;
      #entryFnName = null;
      
      #onResolveHandlers = [];
      #completionPromise = null;
      #rejected = false;
      
      #exitPromise = null;
      #onExitHandlers = [];
      
      #memoryIdx = null;
      #memory = null;
      
      #callbackFn = null;
      #callbackFnName = null;
      
      #postReturnFn = null;
      
      #getCalleeParamsFn = null;
      
      #stringEncoding = null;
      
      #parentSubtask = null;
      
      #errHandling;
      
      #backpressurePromise;
      #backpressureWaiters = 0n;
      
      #returnLowerFns = null;
      
      #subtasks = [];
      
      #entered = false;
      #exited = false;
      #errored = null;
      
      cancelled = false;
      cancelRequested = false;
      alwaysTaskReturn = false;
      
      returnCalls =  0;
      storage = [0, 0];
      borrowedHandles = {};
      
      tmpRetI64HighBits = 0|0;
      
      constructor(opts) {
        this.#id = ++AsyncTask._ID;
        
        if (opts?.componentIdx === undefined) {
          throw new TypeError('missing component id during task creation');
        }
        this.#componentIdx = opts.componentIdx;
        
        this.#state = AsyncTask.State.INITIAL;
        this.#isAsync = opts?.isAsync ?? false;
        this.#isManualAsync = opts?.isManualAsync ?? false;
        this.#entryFnName = opts.entryFnName;
        
        const {
          promise: completionPromise,
          resolve: resolveCompletionPromise,
          reject: rejectCompletionPromise,
        } = promiseWithResolvers();
        this.#completionPromise = completionPromise;
        
        this.#onResolveHandlers.push((results) => {
          if (this.#errored !== null) {
            rejectCompletionPromise(this.#errored);
            return;
          } else if (this.#rejected) {
            rejectCompletionPromise(results);
            return;
          }
          resolveCompletionPromise(results);
        });
        
        const {
          promise: exitPromise,
          resolve: resolveExitPromise,
          reject: rejectExitPromise,
        } = promiseWithResolvers();
        this.#exitPromise = exitPromise;
        
        this.#onExitHandlers.push(() => {
          resolveExitPromise();
        });
        
        if (opts.callbackFn) { this.#callbackFn = opts.callbackFn; }
        if (opts.callbackFnName) { this.#callbackFnName = opts.callbackFnName; }
        
        if (opts.getCalleeParamsFn) { this.#getCalleeParamsFn = opts.getCalleeParamsFn; }
        
        if (opts.stringEncoding) { this.#stringEncoding = opts.stringEncoding; }
        
        if (opts.parentSubtask) { this.#parentSubtask = opts.parentSubtask; }
        
        
        if (opts.errHandling) { this.#errHandling = opts.errHandling; }
      }
      
      taskState() { return this.#state; }
      id() { return this.#id; }
      componentIdx() { return this.#componentIdx; }
      entryFnName() { return this.#entryFnName; }
      
      completionPromise() { return this.#completionPromise; }
      exitPromise() { return this.#exitPromise; }
      
      isAsync() { return this.#isAsync; }
      isSync() { return !this.isAsync(); }
      
      getErrHandling() { return this.#errHandling; }
      
      hasCallback() { return this.#callbackFn !== null; }
      
      getReturnMemoryIdx() { return this.#memoryIdx; }
      setReturnMemoryIdx(idx) {
        if (idx === null) { return; }
        this.#memoryIdx = idx;
      }
      
      getReturnMemory() { return this.#memory; }
      setReturnMemory(m) {
        if (m === null) { return; }
        this.#memory = m;
      }
      
      setReturnLowerFns(fns) { this.#returnLowerFns = fns; }
      getReturnLowerFns() { return this.#returnLowerFns; }
      
      setParentSubtask(subtask) {
        if (!subtask || !(subtask instanceof AsyncSubtask)) { return }
        if (this.#parentSubtask) { throw new Error('parent subtask can only be set once'); }
        this.#parentSubtask = subtask;
      }
      
      getParentSubtask() { return this.#parentSubtask; }
      
      // TODO(threads): this is very inefficient, we can pass along a root task,
      // and ideally do not need this once thread support is in place
      getRootTask() {
        let currentSubtask = this.getParentSubtask();
        let task = this;
        while (currentSubtask) {
          task = currentSubtask.getParentTask();
          currentSubtask = task.getParentSubtask();
        }
        return task;
      }
      
      setPostReturnFn(f) {
        if (!f) { return; }
        if (this.#postReturnFn) { throw new Error('postReturn fn can only be set once'); }
        this.#postReturnFn = f;
      }
      
      setCallbackFn(f, name) {
        if (!f) { return; }
        if (this.#callbackFn) { throw new Error('callback fn can only be set once'); }
        this.#callbackFn = f;
        this.#callbackFnName = name;
      }
      
      getCallbackFnName() {
        if (!this.#callbackFnName) { return undefined; }
        return this.#callbackFnName;
      }
      
      async runCallbackFn(...args) {
        if (!this.#callbackFn) { throw new Error('no callback function has been set for task'); }
        return _withGlobalCurrentTaskMetaAsync({
          taskID: this.#id,
          componentIdx: this.#componentIdx,
          fn: () => { return this.#callbackFn.apply(null, args); }
        });
      }
      
      getCalleeParams() {
        if (!this.#getCalleeParamsFn) { throw new Error('missing/invalid getCalleeParamsFn'); }
        return this.#getCalleeParamsFn();
      }
      
      mayBlock() { return this.isAsync() || this.isResolvedState() }
      
      mayEnter(task) {
        const cstate = getOrCreateAsyncState(this.#componentIdx);
        if (cstate.hasBackpressure()) {
          _debugLog('[AsyncTask#mayEnter()] disallowed due to backpressure', { taskID: this.#id });
          return false;
        }
        if (!cstate.callingSyncImport()) {
          _debugLog('[AsyncTask#mayEnter()] disallowed due to sync import call', { taskID: this.#id });
          return false;
        }
        const callingSyncExportWithSyncPending = cstate.callingSyncExport && !task.isAsync;
        if (!callingSyncExportWithSyncPending) {
          _debugLog('[AsyncTask#mayEnter()] disallowed due to sync export w/ sync pending', { taskID: this.#id });
          return false;
        }
        return true;
      }
      
      enterSync() {
        if (this.needsExclusiveLock()) {
          const cstate = getOrCreateAsyncState(this.#componentIdx);
          // TODO(???): it is *very possible* for a the line below to fail if
          // an async function is already running (and holding the exclusive lock)
          //
          // It's not really possible to fix this unless we turn every sync export into
          // an async export that will use the regular async enabled `enter()`.
          cstate.exclusiveLock();
        }
        return true;
      }
      
      async enter(opts) {
        _debugLog('[AsyncTask#enter()] args', {
          taskID: this.#id,
          componentIdx: this.#componentIdx,
          subtaskID: this.getParentSubtask()?.id(),
          entryFnName: this.#entryFnName,
        });
        
        if (this.#entered) {
          throw new Error(`task with ID [${this.#id}] should not be entered twice`);
        }
        
        const cstate = getOrCreateAsyncState(this.#componentIdx);
        
        await cstate.nextTaskExecutionSlot({ task: this });
        
        // If a task is either synchronous or host-provided (e.g. a host import, whether sync or async)
        // then we can avoid component-relevant tracking and immediately enter
        if (this.isSync() || opts?.isHost) {
          this.#entered = true;
          
          // TODO(breaking): remove once manually-specifying async fns is removed
          // It is currently possible for an actually sync export to be specified
          // as async via JSPI
          if (this.#isManualAsync) {
            if (this.needsExclusiveLock()) { cstate.exclusiveLock(); }
          }
          
          return this.#entered;
        }
        
        // Perform intial backpressure check
        if (cstate.hasBackpressure() || this.needsExclusiveLock() && cstate.isExclusivelyLocked()) {
          cstate.addBackpressureWaiter();
          
          const result = await this.waitUntil({
            readyFn: () => {
              return !(cstate.hasBackpressure()
              || this.needsExclusiveLock() && cstate.isExclusivelyLocked());
            },
            cancellable: true,
          });
          
          cstate.removeBackpressureWaiter();
          
          if (result === AsyncTask.BlockResult.CANCELLED) {
            this.cancel();
            return false;
          }
        }
        
        // Lock the component state or keep trying until we can/do
        try {
          if (this.needsExclusiveLock()) { cstate.exclusiveLock(); }
        } catch {
          // Continuously attempt to lock until we can
          while (cstate.hasBackpressure() || this.needsExclusiveLock() && cstate.isExclusivelyLocked()) {
            try {
              if (this.needsExclusiveLock()) { cstate.exclusiveLock(); }
              break;
            } catch(err) {
              cstate.addBackpressureWaiter();
              const result = await this.waitUntil({
                readyFn: () => {
                  return !(cstate.hasBackpressure()
                  || this.needsExclusiveLock() && cstate.isExclusivelyLocked());
                },
                cancellable: true,
              });
              cstate.removeBackpressureWaiter();
              if (result === AsyncTask.BlockResult.CANCELLED) {
                this.cancel();
                return false;
              }
            }
          }
        }
        
        this.#entered = true;
        return this.#entered;
      }
      
      isRunningState() { return this.#state !== AsyncTask.State.RESOLVED; }
      isResolvedState() { return this.#state === AsyncTask.State.RESOLVED; }
      isResolved() { return this.#state === AsyncTask.State.RESOLVED; }
      
      async waitUntil(opts) {
        const { readyFn, cancellable } = opts;
        _debugLog('[AsyncTask#waitUntil()] args', { taskID: this.#id, cancellable });
        
        // TODO(fix): check for cancel
        // TODO(fix): determinism
        // TODO(threads): add this thread to waiting list
        
        const keepGoing = await this.suspendUntil({
          readyFn,
          cancellable,
        });
        
        return keepGoing;
      }
      
      async yieldUntil(opts) {
        const { readyFn, cancellable } = opts;
        _debugLog('[AsyncTask#yieldUntil()] args', { taskID: this.#id, cancellable });
        
        const keepGoing = await this.suspendUntil({ readyFn, cancellable });
        if (keepGoing) {
          return {
            code: ASYNC_EVENT_CODE.NONE,
            payload0: 0,
            payload1: 0,
          };
        }
        
        return {
          code: ASYNC_EVENT_CODE.TASK_CANCELLED,
          payload0: 0,
          payload1: 0,
        };
      }
      
      async suspendUntil(opts) {
        const { cancellable, readyFn } = opts;
        _debugLog('[AsyncTask#suspendUntil()] args', { cancellable });
        
        const pendingCancelled = this.deliverPendingCancel({ cancellable });
        if (pendingCancelled) { return false; }
        
        const completed = await this.immediateSuspendUntil({ readyFn, cancellable });
        return completed;
      }
      
      // TODO(threads): equivalent to thread.suspend_until()
      async immediateSuspendUntil(opts) {
        const { cancellable, readyFn } = opts;
        _debugLog('[AsyncTask#immediateSuspendUntil()] args', { cancellable, readyFn });
        
        const ready = readyFn();
        if (ready && ASYNC_DETERMINISM === 'random') {
          const coinFlip = _coinFlip();
          if (coinFlip) { return true }
        }
        
        const keepGoing = await this.immediateSuspend({ cancellable, readyFn });
        return keepGoing;
      }
      
      async immediateSuspend(opts) { // NOTE: equivalent to thread.suspend()
      // TODO(threads): store readyFn on the thread
      const { cancellable, readyFn } = opts;
      _debugLog('[AsyncTask#immediateSuspend()] args', { cancellable, readyFn });
      
      const pendingCancelled = this.deliverPendingCancel({ cancellable });
      if (pendingCancelled) { return false; }
      
      const cstate = getOrCreateAsyncState(this.#componentIdx);
      const keepGoing = await cstate.suspendTask({ task: this, readyFn });
      return keepGoing;
    }
    
    deliverPendingCancel(opts) {
      const { cancellable } = opts;
      _debugLog('[AsyncTask#deliverPendingCancel()] args', { cancellable });
      
      if (cancellable && this.#state === AsyncTask.State.PENDING_CANCEL) {
        this.#state = AsyncTask.State.CANCEL_DELIVERED;
        return true;
      }
      
      return false;
    }
    
    isCancelled() { return this.cancelled }
    
    cancel(args) {
      _debugLog('[AsyncTask#cancel()] args', { });
      if (this.taskState() !== AsyncTask.State.CANCEL_DELIVERED) {
        throw new Error(`(component [${this.#componentIdx}]) task [${this.#id}] invalid task state [${this.taskState()}] for cancellation`);
      }
      if (this.borrowedHandles.length > 0) { throw new Error('task still has borrow handles'); }
      this.cancelled = true;
      this.onResolve(args?.error ?? new Error('task cancelled'));
      this.#state = AsyncTask.State.RESOLVED;
    }
    
    onResolve(taskValue) {
      const handlers = this.#onResolveHandlers;
      this.#onResolveHandlers = [];
      for (const f of handlers) {
        try {
          // TODO(fix): resolve handlers getting called a ton?
          f(taskValue);
        } catch (err) {
          _debugLog("[AsyncTask#onResolve] error during task resolve handler", err);
          throw err;
        }
      }
      
      if (this.#parentSubtask) {
        const meta = this.#parentSubtask.getCallMetadata();
        // Run the rturn fn if it has not already been called -- this *should* have happened in
        // `task.return`, but some paths do not go through task.return (e.g. async lower of sync fn
        // which goes through prepare + async-start-call)
        if (meta.returnFn && !meta.returnFnCalled) {
          _debugLog('[AsyncTask#onResolve()] running returnFn', {
            componentIdx: this.#componentIdx,
            taskID: this.#id,
            subtaskID: this.#parentSubtask.id(),
          });
          const memory = meta.getMemoryFn();
          meta.returnFn.apply(null, [taskValue, meta.resultPtr]);
          meta.returnFnCalled = true;
        }
      }
      
      if (this.#postReturnFn) {
        _debugLog('[AsyncTask#onResolve()] running post return ', {
          componentIdx: this.#componentIdx,
          taskID: this.#id,
        });
        try {
          this.#postReturnFn(taskValue);
        } catch (err) {
          _debugLog("[AsyncTask#onResolve] error during task resolve handler", err);
          throw err;
        }
      }
      
      if (this.#parentSubtask) {
        this.#parentSubtask.onResolve(taskValue);
      }
    }
    
    registerOnResolveHandler(f) {
      this.#onResolveHandlers.push(f);
    }
    
    isRejected() { return this.#rejected; }
    
    setErrored(err) {
      this.#errored = err;
    }
    
    reject(taskErr) {
      _debugLog('[AsyncTask#reject()] args', {
        componentIdx: this.#componentIdx,
        taskID: this.#id,
        parentSubtask: this.#parentSubtask,
        parentSubtaskID: this.#parentSubtask?.id(),
        entryFnName: this.entryFnName(),
        callbackFnName: this.#callbackFnName,
        errMsg: taskErr.message,
      });
      
      if (this.isResolvedState() || this.#rejected) { return; }
      
      for (const subtask of this.#subtasks) {
        subtask.reject(taskErr);
      }
      
      this.#rejected = true;
      this.cancelRequested = true;
      this.#state = AsyncTask.State.PENDING_CANCEL;
      const cancelled = this.deliverPendingCancel({ cancellable: true });
      
      // TODO: do cleanup here to reset the machinery so we can run again?
      
      this.cancel({ error: taskErr });
    }
    
    resolve(results) {
      _debugLog('[AsyncTask#resolve()] args', {
        componentIdx: this.#componentIdx,
        taskID: this.#id,
        entryFnName: this.entryFnName(),
        callbackFnName: this.#callbackFnName,
      });
      
      if (this.#state === AsyncTask.State.RESOLVED) {
        throw new Error(`(component [${this.#componentIdx}]) task [${this.#id}]  is already resolved (did you forget to wait for an import?)`);
      }
      
      if (this.borrowedHandles.length > 0) {
        throw new Error('task still has borrow handles');
      }
      
      this.#state = AsyncTask.State.RESOLVED;
      
      switch (results.length) {
        case 0:
        this.onResolve(undefined);
        break;
        case 1:
        this.onResolve(results[0]);
        break;
        default:
        _debugLog('[AsyncTask#resolve()] unexpected number of results', {
          componentIdx: this.#componentIdx,
          results,
          taskID: this.#id,
          subtaskID: this.#parentSubtask?.id(),
          entryFnName: this.#entryFnName,
          callbackFnName: this.#callbackFnName,
        });
        throw new Error('unexpected number of results');
      }
    }
    
    exit(args) {
      _debugLog('[AsyncTask#exit()]', {
        componentIdx: this.#componentIdx,
        taskID: this.#id,
      });
      
      if (this.#exited)  { throw new Error("task has already exited"); }
      
      if (this.#state !== AsyncTask.State.RESOLVED) {
        // TODO(fix): only fused, manually specified post returns seem to break this invariant,
        // as the TaskReturn trampoline is not activated it seems.
        //
        // see: test/p3/ported/wasmtime/component-async/post-return.js
        //
        // We *should* be able to upgrade this to be more strict and throw at some point,
        // which may involve rewriting the upstream test to surface task return manually somehow.
        //
        //throw new Error(`(component [${this.#componentIdx}]) task [${this.#id}] exited without resolution`);
        _debugLog('[AsyncTask#exit()] task exited without resolution', {
          componentIdx: this.#componentIdx,
          taskID: this.#id,
          subtask: this.getParentSubtask(),
          subtaskID: this.getParentSubtask()?.id(),
        });
        this.#state = AsyncTask.State.RESOLVED;
      }
      
      if (this.borrowedHandles > 0) {
        throw new Error('task [${this.#id}] exited without clearing borrowed handles');
      }
      
      const state = getOrCreateAsyncState(this.#componentIdx);
      if (!state) { throw new Error('missing async state for component [' + this.#componentIdx + ']'); }
      
      // Exempt the host from exclusive lock check
      if (this.#componentIdx !== -1 && !args?.skipExclusiveLockCheck) {
        if (this.needsExclusiveLock() && !state.isExclusivelyLocked()) {
          throw new Error(`task [${this.#id}] exit: component [${this.#componentIdx}] should have been exclusively locked`);
        }
      }
      
      state.exclusiveRelease();
      
      for (const f of this.#onExitHandlers) {
        try {
          f();
        } catch (err) {
          console.error("error during task exit handler", err);
          throw err;
        }
      }
      
      this.#exited = true;
      clearCurrentTask(this.#componentIdx, this.id());
    }
    
    needsExclusiveLock() {
      return !this.#isAsync || this.hasCallback();
    }
    
    createSubtask(args) {
      _debugLog('[AsyncTask#createSubtask()] args', args);
      const { componentIdx, childTask, callMetadata, fnName, isAsync, isManualAsync } = args;
      
      const cstate = getOrCreateAsyncState(this.#componentIdx);
      if (!cstate) {
        throw new Error(`invalid/missing async state for component idx [${componentIdx}]`);
      }
      
      const waitable = new Waitable({
        componentIdx: this.#componentIdx,
        target: `subtask (internal ID [${this.#id}])`,
      });
      
      const newSubtask = new AsyncSubtask({
        componentIdx,
        childTask,
        parentTask: this,
        callMetadata,
        isAsync,
        isManualAsync,
        fnName,
        waitable,
      });
      this.#subtasks.push(newSubtask);
      newSubtask.setTarget(`subtask (internal ID [${newSubtask.id()}], waitable [${waitable.idx()}], component [${componentIdx}])`);
      waitable.setIdx(cstate.handles.insert(newSubtask));
      waitable.setTarget(`waitable for subtask (waitable id [${waitable.idx()}], subtask internal ID [${newSubtask.id()}])`);
      
      return newSubtask;
    }
    
    getLatestSubtask() {
      return this.#subtasks.at(-1);
    }
    
    getSubtaskByWaitableRep(rep) {
      if (rep === undefined) { throw new TypeError('missing rep'); }
      return this.#subtasks.find(s => s.waitableRep() === rep);
    }
    
    currentSubtask() {
      _debugLog('[AsyncTask#currentSubtask()]');
      if (this.#subtasks.length === 0) { return undefined; }
      return this.#subtasks.at(-1);
    }
    
    removeSubtask(subtask) {
      if (this.#subtasks.length === 0) { throw new Error('cannot end current subtask: no current subtask'); }
      this.#subtasks = this.#subtasks.filter(t => t !== subtask);
      return subtask;
    }
  }
  
  async function _driverLoop(args) {
    _debugLog('[_driverLoop()] args', args);
    const {
      componentState,
      task,
      fnName,
      isAsync,
    } = args;
    let callbackResult = args.callbackResult;
    
    const callbackFnName = task.getCallbackFnName();
    const componentIdx = task.componentIdx();
    
    if (callbackResult instanceof Promise) {
      throw new Error("callbackResult should be a value, not a promise");
    }
    
    if (callbackResult === undefined) {
      throw new Error("callback result should never be undefined");
    }
    
    let callbackCode;
    let waitableSetRep;
    let unpacked;
    try {
      if (!(_typeCheckValidI32(callbackResult))) {
        throw new Error('invalid callback result [' + callbackResult + '], not a number');
      }
      
      unpacked = unpackCallbackResult(callbackResult);
      callbackCode = unpacked[0];
      waitableSetRep = unpacked[1];
    } catch(err) {
      console.error("failed to unpack callback result", err);
      throw err;
    }
    
    if (callbackCode < 0 || callbackCode > 3) {
      throw new Error('invalid async return value, outside callback code range');
    }
    
    const cstate = getOrCreateAsyncState(componentIdx);
    
    let eventCode;
    let index;
    let result;
    let asyncRes;
    let wset;
    try {
      while (true) {
        if (callbackCode !== 0) { componentState.exclusiveRelease(); }
        
        switch (callbackCode) {
          case 0: // EXIT
          _debugLog('[_driverLoop()] async exit indicated', {
            fnName,
            componentIdx,
            callbackFnName,
            taskID: task.id()
          });
          task.exit({ skipExclusiveLockCheck: true });
          return;
          
          case 1: // YIELD
          _debugLog('[_driverLoop()] yield', {
            fnName,
            componentIdx,
            callbackFnName,
            taskID: task.id()
          });
          asyncRes = await task.yieldUntil({
            cancellable: true,
            readyFn: () => !componentState.isExclusivelyLocked(),
          });
          _debugLog('[_driverLoop()] finished yield', {
            fnName,
            componentIdx,
            callbackFnName,
            taskID: task.id(),
            asyncRes,
          });
          break;
          
          case 2: // WAIT for a given waitable set
          _debugLog('[_driverLoop()] waiting for event', {
            fnName,
            componentIdx,
            callbackFnName,
            taskID: task.id(),
            waitableSetRep,
            waitableSetTargets: cstate.handles.get(waitableSetRep).targets(),
          });
          
          wset = cstate.handles.get(waitableSetRep);
          if (!(wset instanceof WaitableSet)) {
            throw new Error(`non-waitable set returned from component state handles @ [${waitableSetRep}]`);
          }
          
          asyncRes = await wset.waitUntil({
            readyFn: () => !componentState.isExclusivelyLocked(),
            task,
            cancellable: true,
          });
          
          _debugLog('[_driverLoop()] finished waiting for event', {
            fnName,
            componentIdx,
            callbackFnName,
            taskID: task.id(),
            waitableSetRep,
            asyncRes,
          });
          
          break;
          
          default:
          throw new Error(`Unrecognized async function result [${ret}]`);
        }
        
        componentState.exclusiveLock();
        
        // If the task failed via any means, leave early and reject.
        if (task.isRejected()) {
          _debugLog('[_driverLoop()] detected task rejection, leaving early');
          return;
        }
        
        if (asyncRes.code === undefined) { throw new Error("missing event code from event"); }
        if (asyncRes.payload0 === undefined) { throw new Error("missing payload0 from event"); }
        if (asyncRes.payload1 === undefined) { throw new Error("missing payload1 from event"); }
        
        eventCode = asyncRes.code; // async event enum code
        index = asyncRes.payload0; // varies (e.g. idx of related waitable set)
        result = asyncRes.payload1; // varies (e.g. task state)
        asyncRes = null;
        
        _debugLog('[_driverLoop()] performing callback', {
          fnName,
          componentIdx,
          taskID: task.id(),
          callbackFnName,
          eventCode,
          index,
          result
        });
        
        const callbackRes = await task.runCallbackFn(
        toInt32(eventCode),
        toInt32(index),
        toInt32(result),
        );
        
        unpacked = unpackCallbackResult(callbackRes);
        callbackCode = unpacked[0];
        waitableSetRep = unpacked[1];
        
        _debugLog('[_driverLoop()] callback result unpacked', {
          fnName,
          componentIdx,
          callbackFnName,
          callbackRes,
          callbackCode,
          waitableSetRep,
        });
      }
    } catch (err) {
      _debugLog('[_driverLoop()] error during async driver loop', {
        fnName,
        callbackFnName,
        componentIdx,
        taskID: task.id(),
        subtaskID: task.getParentSubtask()?.id(),
        parentTaskID: task.getParentSubtask()?.getParentTask()?.id(),
        event: {
          eventCode,
          index,
          result,
        },
        err,
      });
    }
  }
  
  async function _lowerImport(args) {
    const params = [...arguments].slice(1);
    _debugLog('[_lowerImport()] args', { args, params });
    const {
      functionIdx,
      componentIdx,
      isAsync,
      isManualAsync,
      paramLiftFns,
      resultLowerFns,
      hasResultPointer,
      funcTypeIsAsync,
      metadata,
      memoryIdx,
      getMemoryFn,
      getReallocFn,
      stringEncoding,
      importFn,
    } = args;
    
    const { taskID } = _getGlobalCurrentTaskMeta(componentIdx);
    
    const taskMeta = getCurrentTask(componentIdx, taskID);
    if (!taskMeta) { throw new Error('invalid/missing async task meta'); }
    
    const task = taskMeta.task;
    if (!task) { throw new Error('invalid/missing async task'); }
    
    const cstate = getOrCreateAsyncState(componentIdx);
    
    // TODO: re-enable this check -- postReturn can call imports though,
    // and that breaks things.
    //
    // if (!cstate.mayLeave) {
      //     throw new Error(`cannot leave instance [${componentIdx}]`);
      // }
      
      if (!task.mayBlock() && funcTypeIsAsync && !isAsync) {
        throw new Error("non async exports cannot synchronously call async functions");
      }
      
      // If there is an existing task, this should be part of a subtask
      const memory = getMemoryFn();
      // Canonical ABI lower appends result storage as a trailing
      // param when async lower has any flat result, or sync lower
      // has more than one flat result.
      const resultPtr = hasResultPointer ? params[params.length - 1] : undefined;
      const subtask = task.createSubtask({
        componentIdx,
        parentTask: task,
        fnName: importFn.fnName,
        isAsync,
        isManualAsync,
        callMetadata: {
          memoryIdx,
          memory,
          realloc: getReallocFn?.(),
          getReallocFn,
          resultPtr,
          lowers: resultLowerFns,
          stringEncoding,
        }
      });
      task.setReturnMemoryIdx(memoryIdx);
      task.setReturnMemory(getMemoryFn());
      
      subtask.onStart();
      
      // If dealing with a sync lowered sync function, we can directly return results
      //
      // TODO(breaking): remove once we get rid of manual async import specification,
      // as func types cannot be detected in that case only (and we don't need that w/ p3)
      if (!isManualAsync && !isAsync && !funcTypeIsAsync) {
        const res = importFn(...params);
        // TODO(breaking): remove once we get rid of manual async import specification,
        // as func types cannot be detected in that case only (and we don't need that w/ p3)
        if (!funcTypeIsAsync && !subtask.isReturned()) {
          throw new Error('post-execution subtasks must either be async or returned');
        }
        return subtask.getResult();
      }
      
      // Sync-lowered async functions requires async behavior because the callee *can* block,
      // but this call must *act* synchronously and return immediately with the result
      // (i.e. not returning until the work is done)
      //
      // TODO(breaking): remove checking for manual async specification here, once we can go p3-only
      //
      if (!isManualAsync && !isAsync && funcTypeIsAsync) {
        const { promise, resolve } = new Promise();
        queueMicrotask(async () => {
          if (!subtask.isResolvedState()) {
            await task.suspendUntil({ readyFn: () => task.isResolvedState() });
          }
          resolve(subtask.getResult());
        });
        return promise;
      }
      
      // NOTE: at this point we know that we are working with an async lowered import
      
      subtask.setOnProgressFn(() => {
        subtask.setPendingEvent(() => {
          if (subtask.isResolved()) { subtask.deliverResolve(); }
          const event = {
            code: ASYNC_EVENT_CODE.SUBTASK,
            payload0: subtask.waitableRep(),
            payload1: subtask.getStateNumber(),
          }
          return event;
        });
      });
      
      // This is a hack to maintain backwards compatibility with
      // manually-specified async imports, used in wasm exports that are
      // not actually async (but are specified as so).
      //
      // This is not normal p3 sync behavior but instead anticipating that
      // the caller that is doing manual async will be waiting for a promise that
      // resolves to the *actual* result.
      //
      // TODO(breaking): remove once manually specified async is removed
      //
      // There are a few cases:
      // 1. sync function with async types (e.g. `f: func() -> stream<u32>`)
      // 2. async function with async types (e.g. `f: async func() -> stream<u32>`)
      // 3. async function with sync types (e.g. `f: async func() -> list<u32>`)
      // 4. sync function with non-async types (e.g. `f: func() -> list<u32>`)
      //
      // This hack *only* applies to 4 -- the case where an async JS host function
      // is supplied to a Wasm export which does *not* need to do any async abi
      // lifting/lowering (async ABI did not exist when JSPI integratiton was
      // initially merged to enable asynchronously returning values from the host)
      //
      const requiresManualAsyncResult = !isAsync && !funcTypeIsAsync && isManualAsync;
      let manualAsyncResult;
      if (requiresManualAsyncResult) {
        manualAsyncResult = promiseWithResolvers();
      }
      
      try {
        _debugLog('[_lowerImport()] calling lowered import', { importFn, params });
        const res = await importFn(...params);
        if (requiresManualAsyncResult) {
          manualAsyncResult.resolve(subtask.getResult());
        }
      } catch (err) {
        _debugLog("[_lowerImport()] import fn error:", err);
        if (requiresManualAsyncResult) {
          manualAsyncResult.reject(err);
        }
        throw err;
      }
      
      if (requiresManualAsyncResult) { return manualAsyncResult.promise; }
      
      const subtaskState = subtask.getStateNumber();
      if (subtaskState < 0 || subtaskState >= 2**4) {
        throw new Error('invalid subtask state, out of valid range');
      }
      
      // An async-lowered import whose callee resolved synchronously returns
      // [Subtask.State.RETURNED] only an no subtask handle is exposed.
      if (subtask.isReturned()) {
        if (!subtask.resolveDelivered()) {
          subtask.deliverResolve();
        }
        const removed = cstate.handles.remove(subtask.waitableRep());
        if (removed !== subtask) {
          throw new Error('subtask handle cleanup removed unexpected entry');
        }
        return subtaskState;
      }
      
      return Number(subtask.waitableRep()) << 4 | subtaskState;
    }
    
    function _lowerImportBackwardsCompat(args) {
      const params = [...arguments].slice(1);
      _debugLog('[_lowerImportBackwardsCompat()] args', { args, params });
      const {
        functionIdx,
        componentIdx,
        isAsync,
        isManualAsync,
        paramLiftFns,
        resultLowerFns,
        hasResultPointer,
        funcTypeIsAsync,
        metadata,
        memoryIdx,
        getMemoryFn,
        getReallocFn,
        importFn,
        stringEncoding,
      } = args;
      
      let meta = _getGlobalCurrentTaskMeta(componentIdx);
      let createdTask;
      
      // Some components depend on initialization logic (i.e. `_initialize` or some such
      // core wasm export) that is embedded in the component, but is not executed or wizer'd
      // away before the transpiled component is attempted to be used.
      //
      // These components execut their initialization logic *when they are imported* in the
      // transpiled context -- so we may get a call to an export that is lowered without going
      // through `CallWasm` or `CallInterface`.
      //
      if (!meta) {
        if (funcTypeIsAsync || (isAsync && !isManualAsync)) {
          throw new Error('p3 async wasm exports cannot use backwards compat auto-task init');
        }
        
        const [newTask, newTaskID] = createNewCurrentTask({
          componentIdx,
          isAsync,
          isManualAsync,
          callingWasmExport: false,
        });
        createdTask = newTask;
        
        // Since we're managing the task creation ourselves we must clear ourselves
        createdTask.registerOnResolveHandler(() => {
          _clearCurrentTask({
            taskID: task.id(),
            componentIdx: task.componentIdx(),
          });
        });
        
        _setGlobalCurrentTaskMeta({
          componentIdx,
          taskID: newTaskID,
        });
        
        meta = _getGlobalCurrentTaskMeta(componentIdx);
      }
      
      const { taskID } = meta;
      
      const taskMeta = getCurrentTask(componentIdx, taskID);
      if (!taskMeta) {
        throw new Error('invalid/missing async task meta');
      }
      
      const task = taskMeta.task;
      if (!task) { throw new Error('invalid/missing async task'); }
      
      const cstate = getOrCreateAsyncState(componentIdx);
      
      // TODO: re-enable this check -- postReturn can call imports though,
      // and that breaks things.
      //
      // if (!cstate.mayLeave) {
        //     throw new Error(`cannot leave instance [${componentIdx}]`);
        // }
        
        if (!task.mayBlock() && funcTypeIsAsync && !isAsync) {
          throw new Error("non async exports cannot synchronously call async functions");
        }
        
        // If there is an existing task, this should be part of a subtask
        const memory = getMemoryFn();
        // Canonical ABI lower appends result storage as a trailing
        // param when async lower has any flat result, or sync lower
        // has more than one flat result.
        const resultPtr = hasResultPointer ? params[params.length - 1] : undefined;
        const subtask = task.createSubtask({
          componentIdx,
          parentTask: task,
          fnName: importFn.fnName,
          isAsync,
          isManualAsync,
          callMetadata: {
            memoryIdx,
            memory,
            realloc: getReallocFn?.(),
            getReallocFn,
            resultPtr,
            lowers: resultLowerFns,
            stringEncoding,
          }
        });
        task.setReturnMemoryIdx(memoryIdx);
        task.setReturnMemory(getMemoryFn());
        
        subtask.onStart();
        
        // If dealing with a sync lowered sync function, we can directly return results
        //
        // TODO(breaking): remove once we get rid of manual async import specification,
        // as func types cannot be detected in that case only (and we don't need that w/ p3)
        if (!isManualAsync && !isAsync && !funcTypeIsAsync) {
          if (createdTask) { createdTask.enterSync(); }
          
          const res = importFn(...params);
          
          // TODO(breaking): remove once we get rid of manual async import specification,
          // as func types cannot be detected in that case only (and we don't need that w/ p3)
          if (!funcTypeIsAsync && !subtask.isReturned()) {
            throw new Error('post-execution subtasks must either be async or returned');
          }
          
          const syncRes = subtask.getResult();
          if (createdTask) { createdTask.resolve([syncRes]); }
          
          return syncRes;
        }
        
        // Sync-lowered async functions requires async behavior because the callee *can* block,
        // but this call must *act* synchronously and return immediately with the result
        // (i.e. not returning until the work is done)
        //
        // TODO(breaking): remove checking for manual async specification here, once we can go p3-only
        //
        if (!isManualAsync && !isAsync && funcTypeIsAsync) {
          const { promise, resolve } = new Promise();
          queueMicrotask(async () => {
            if (!subtask.isResolvedState()) {
              await task.suspendUntil({ readyFn: () => task.isResolvedState() });
            }
            resolve(subtask.getResult());
          });
          return promise;
        }
        
        // NOTE: at this point we know that we are working with an async lowered import
        
        const subtaskState = subtask.getStateNumber();
        if (subtaskState < 0 || subtaskState >= 2**4) {
          throw new Error('invalid subtask state, out of valid range');
        }
        
        subtask.setOnProgressFn(() => {
          subtask.setPendingEvent(() => {
            if (subtask.isResolved()) { subtask.deliverResolve(); }
            const event = {
              code: ASYNC_EVENT_CODE.SUBTASK,
              payload0: subtask.waitableRep(),
              payload1: subtask.getStateNumber(),
            }
            return event;
          });
        });
        
        // This is a hack to maintain backwards compatibility with
        // manually-specified async imports, used in wasm exports that are
        // not actually async (but are specified as so).
        //
        // This is not normal p3 sync behavior but instead anticipating that
        // the caller that is doing manual async will be waiting for a promise that
        // resolves to the *actual* result.
        //
        // TODO(breaking): remove once manually specified async is removed
        //
        // There are a few cases:
        // 1. sync function with async types (e.g. `f: func() -> stream<u32>`)
        // 2. async function with async types (e.g. `f: async func() -> stream<u32>`)
        // 3. async function with sync types (e.g. `f: async func() -> list<u32>`)
        // 4. sync function with non-async types (e.g. `f: func() -> list<u32>`)
        //
        // This hack *only* applies to 4 -- the case where an async JS host function
        // is supplied to a Wasm export which does *not* need to do any async abi
        // lifting/lowering (async ABI did not exist when JSPI integratiton was
        // initially merged to enable asynchronously returning values from the host)
        //
        const requiresManualAsyncResult = !isAsync && !funcTypeIsAsync && isManualAsync;
        let manualAsyncResult;
        if (requiresManualAsyncResult) {
          manualAsyncResult = promiseWithResolvers();
        }
        
        queueMicrotask(async () => {
          try {
            _debugLog('[_lowerImportBackwardsCompat()] calling lowered import', { importFn, params });
            if (createdTask) { await createdTask.enter(); }
            
            const asyncRes = await importFn(...params);
            if (requiresManualAsyncResult) {
              manualAsyncResult.resolve(subtask.getResult());
            }
            
            if (createdTask) { createdTask.resolve([asyncRes]); }
            
            
          } catch (err) {
            _debugLog("[_lowerImportBackwardsCompat()] import fn error:", err);
            if (requiresManualAsyncResult) {
              manualAsyncResult.reject(err);
            }
            throw err;
          }
        });
        
        if (requiresManualAsyncResult) { return manualAsyncResult.promise; }
        
        return Number(subtask.waitableRep()) << 4 | subtaskState;
      }
      
      class WaitableSet {
        #componentIdx;
        #waitables = [];
        #pendingEvent = null;
        #waiting = 0;
        
        target;
        
        constructor(componentIdx) {
          if (componentIdx === undefined) { throw new TypeError("missing/invalid component idx"); }
          this.#componentIdx = componentIdx;
          this.target = `component [${this.#componentIdx}] waitable set`;
        }
        
        componentIdx() { return this.#componentIdx; }
        
        numWaitables() { return this.#waitables.length; }
        numWaiting() { return this.#waiting; }
        
        incrementNumWaiting(n) { this.#waiting += n ?? 1; }
        decrementNumWaiting(n) { this.#waiting -= n ?? 1; }
        
        targets() { return this.#waitables.map(w => w.target); }
        
        setTarget(tgt) { this.target = tgt; }
        
        shuffleWaitables() {
          this.#waitables = this.#waitables
          .map(value => ({ value, sort: Math.random() }))
          .sort((a, b) => a.sort - b.sort)
          .map(({ value }) => value);
        }
        
        removeWaitable(waitable) {
          const existing = this.#waitables.find(w => w === waitable);
          if (!existing) { return undefined; }
          this.#waitables = this.#waitables.filter(w => w !== waitable);
          return waitable;
        }
        
        addWaitable(waitable) {
          this.removeWaitable(waitable);
          this.#waitables.push(waitable);
        }
        
        hasPendingEvent() {
          _debugLog('[WaitableSet#hasPendingEvent()] args', {
            componentIdx: this.#componentIdx,
            waitableSet: this,
            waitableSetTargets: this.targets(),
          });
          const waitable = this.#waitables.find(w => w.hasPendingEvent());
          return waitable !== undefined;
        }
        
        getPendingEvent() {
          _debugLog('[WaitableSet#getPendingEvent()] args', {
            componentIdx: this.#componentIdx,
            waitableSet: this,
          });
          for (const waitable of this.#waitables) {
            if (!waitable.hasPendingEvent()) { continue; }
            const event = waitable.getPendingEvent();
            _debugLog('[WaitableSet#getPendingEvent()] found pending event', {
              waitable,
              event,
            });
            return event;
          }
          throw new Error('no waitables had a pending event');
        }
        
        async waitUntil(opts) {
          _debugLog('[WaitableSet#waitUntil()] args', { opts });
          // TODO(threads): this task should be the thread
          const { readyFn, task, cancellable } = opts;
          
          let event;
          
          this.incrementNumWaiting();
          
          const keepGoing = await task.suspendUntil({
            readyFn: () => {
              const hasPendingEvent = this.hasPendingEvent();
              const ready = readyFn();
              return ready && hasPendingEvent;
            },
            cancellable,
          });
          
          if (keepGoing) {
            event = this.getPendingEvent();
          } else {
            event = {
              code: ASYNC_EVENT_CODE.TASK_CANCELLED,
              payload0: 0,
              payload1: 0,
            };
          }
          
          this.decrementNumWaiting();
          
          return event;
        }
        
      }
      
      function waitableSetNew(componentIdx) {
        _debugLog('[waitableSetNew()] args', { componentIdx });
        
        const state = getOrCreateAsyncState(componentIdx);
        if (!state) {throw new Error(`missing async state for component idx [${componentIdx}]`); }
        
        const wset = new WaitableSet(componentIdx);
        const rep = state.handles.insert(wset);
        if (typeof rep !== 'number') { throw new Error(`invalid/missing waitable set rep [${rep}]`); }
        
        _debugLog('[waitableSetNew()] created waitable set', { componentIdx, rep });
        return rep;
      }
      
      function waitableSetPoll(ctx, waitableSetRep, resultPtr) {
        const { componentIdx, memoryIdx, getMemoryFn, isAsync, isCancellable } = ctx;
        _debugLog('[waitableSetPoll()] args', {
          componentIdx,
          memoryIdx,
          waitableSetRep,
          resultPtr,
        });
        
        const taskMeta = getCurrentTask(componentIdx);
        if (!taskMeta) { throw Error('invalid/missing current task meta'); }
        if (taskMeta.componentIdx !== componentIdx) {
          throw Error('task component idx [' + task.componentIdx + '] != component instance ID [' + componentIdx + ']');
        }
        
        const task = taskMeta.task;
        if (!task) { throw Error('invalid/missing async task in task meta'); }
        
        if (task.componentIdx() !== componentIdx) {
          throw Error(`task component idx [${task.componentIdx()}] does not match generated [${componentIdx}]`);
        }
        
        const cstate = getOrCreateAsyncState(task.componentIdx());
        const wset = cstate.handles.get(waitableSetRep);
        if (!wset) {
          throw new Error(`missing waitable set [${waitableSetRep}] in component [${componentIdx}]`);
        }
        
        let event;
        const cancelDelivered = task.deliverPendingCancel({ cancellable: isCancellable });
        if (cancelDelivered) {
          event = { code: ASYNC_EVENT_CODE.TASK_CANCELLED, payload0: 0, payload1: 0 };
        } else if (!wset.hasPendingEvent()) {
          event = { code: ASYNC_EVENT_CODE.NONE, payload0: 0, payload1: 0 };
        } else {
          event = wset.getPendingEvent();
        }
        
        const eventCode = _storeEventInComponentMemory({ event, ptr: resultPtr, memory: getMemoryFn() });
        return eventCode;
      }
      
      function waitableSetDrop(componentIdx, waitableSetRep) {
        _debugLog('[waitableSetDrop()] args', { componentIdx, waitableSetRep });
        const task = getCurrentTask(componentIdx);
        
        if (!task) { throw new Error('invalid/missing async task'); }
        if (task.componentIdx !== componentIdx) {
          throw Error('task component idx [' + task.componentIdx + '] != component instance ID [' + componentIdx + ']');
        }
        
        const state = getOrCreateAsyncState(componentIdx);
        if (!state.mayLeave) { throw new Error('component instance is not marked as may leave, cannot be cancelled'); }
        
        _removeWaitableSet({ state, waitableSetRep });
      }
      
      function _removeWaitableSet(args) {
        _debugLog('[_removeWaitableSet()] args', args);
        const { state, waitableSetRep } = args;
        if (!state) { throw new TypeError("missing component state"); }
        if (!waitableSetRep) { throw new TypeError("missing component waitableSetRep"); }
        
        const ws = state.handles.get(waitableSetRep);
        if (!ws) {
          throw new Error('cannot remove waitable set: no set present with rep [' + waitableSetRep + ']');
        }
        if (ws.hasPendingEvent()) {
          throw new Error('waitable set cannot be removed with pending items remaining');
        }
        
        const waitableSet = state.handles.get(waitableSetRep);
        if (ws.numWaitables() > 0) {
          throw new Error('waitable set still contains waitables');
        }
        if (ws.numWaiting() > 0) {
          throw new Error('waitable set still has other tasks waiting on it');
        }
        
        state.handles.remove(waitableSetRep);
      }
      
      function waitableJoin(componentIdx, waitableRep, waitableSetRep) {
        _debugLog('[waitableJoin()] args', { componentIdx, waitableSetRep, waitableRep });
        
        const state = getOrCreateAsyncState(componentIdx);
        if (!state) {
          throw new Error(`invalid/missing async state for component instance [${componentIdx}]`);
        }
        
        if (!state.mayLeave) {
          throw new Error('component instance is not marked as may leave, cannot join waitable');
        }
        
        const waitableObj = state.handles.get(waitableRep);
        if (!waitableObj) {
          throw new Error(`missing waitable obj (rep [${waitableRep}]), component idx [${componentIdx}])`);
        }
        const waitable = waitableObj.getWaitable ? waitableObj.getWaitable() : waitableObj;
        if (!waitable.join) {
          throw new Error("invalid waitable object, does not have join()");
        }
        
        const waitableSet = waitableSetRep === 0 ? null : state.handles.get(waitableSetRep);
        if (waitableSetRep !== 0 && !waitableSet) {
          throw new Error(`missing waitable set [${waitableSetRep}] in component idx [${componentIdx}]`);
        }
        
        waitable.join(waitableSet);
      }
      
      function _liftFlatU8(ctx) {
        _debugLog('[_liftFlatU8()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length === 0) { throw new Error('expected at least a single i32 argument'); }
          val = ctx.params[0];
          ctx.params = ctx.params.slice(1);
          return [val, ctx];
        }
        
        if (ctx.storageLen !== undefined && ctx.storageLen < 1) {
          throw new Error(`insufficient storage ([${ctx.storageLen}] bytes) for lift (u8 requires 1 byte)`);
        }
        
        val = new DataView(ctx.memory.buffer).getUint8(ctx.storagePtr, true);
        
        ctx.storagePtr += 1;
        if (ctx.storageLen !== undefined) { ctx.storageLen -= 1; }
        
        return [val, ctx];
      }
      
      
      function _liftFlatU16(ctx) {
        _debugLog('[_liftFlatU16()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length === 0) { throw new Error('expected at least a single i32 argument'); }
          val = ctx.params[0];
          ctx.params = ctx.params.slice(1);
          return [val, ctx];
        }
        
        if (ctx.storageLen !== undefined && ctx.storageLen < 2) {
          throw new Error(`insufficient storage ([${ctx.storageLen}] bytes) for lift (u16 requires 2 bytes)`);
        }
        
        val = new DataView(ctx.memory.buffer).getUint16(ctx.storagePtr, true);
        
        ctx.storagePtr += 2;
        if (ctx.storageLen !== undefined) { ctx.storageLen -= 2; }
        
        const rem = ctx.storagePtr % 2;
        if (rem !== 0) { ctx.storagePtr += (2 - rem); }
        
        return [val, ctx];
      }
      
      
      function _liftFlatU32(ctx) {
        _debugLog('[_liftFlatU32()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length === 0) { throw new Error('expected at least a single i34 argument'); }
          val = ctx.params[0];
          ctx.params = ctx.params.slice(1);
          return [val, ctx];
        }
        
        if (ctx.storageLen !== undefined && ctx.storageLen < 4) {
          throw new Error(`insufficient storage ([${ctx.storageLen}] bytes) for lift (u32 requires 4 bytes)`);
        }
        val = new DataView(ctx.memory.buffer).getUint32(ctx.storagePtr, true);
        ctx.storagePtr += 4;
        if (ctx.storageLen !== undefined) { ctx.storageLen -= 4; }
        
        return [val, ctx];
      }
      
      
      function _liftFlatU64(ctx) {
        _debugLog('[_liftFlatU64()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length === 0) { throw new Error('expected at least one single i64 argument'); }
          if (typeof ctx.params[0] !== 'bigint') { throw new Error('expected bigint'); }
          val = ctx.params[0];
          ctx.params = ctx.params.slice(1);
          return [val, ctx];
        }
        
        if (ctx.storageLen !== undefined && ctx.storageLen < 8) {
          throw new Error(`insufficient storage ([${ctx.storageLen}] bytes) for lift (u64 requires 8 bytes)`);
        }
        
        val = new DataView(ctx.memory.buffer).getBigUint64(ctx.storagePtr, true);
        ctx.storagePtr += 8;
        if (ctx.storageLen !== undefined) { ctx.storageLen -= 8; }
        
        return [val, ctx];
      }
      
      
      function _liftFlatStringAny(ctx) {
        switch (ctx.stringEncoding) {
          case 'utf8':
          return _liftFlatStringUTF8(ctx);
          case 'utf16':
          return _liftFlatStringUTF16(ctx);
          default:
          throw new Error(`missing/unrecognized/unsupported string encoding [${ctx.stringEncoding}]`);
        }
      }
      
      function _liftFlatStringUTF8(ctx) {
        _debugLog('[_liftFlatStringUTF8()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length < 2) { throw new Error('expected at least two u32 arguments'); }
          const offset = ctx.params[0];
          if (!Number.isSafeInteger(offset)) {  throw new Error('invalid offset'); }
          const len = ctx.params[1];
          if (!Number.isSafeInteger(len)) {  throw new Error('invalid len'); }
          val = TEXT_DECODER_UTF8.decode(new DataView(ctx.memory.buffer, offset, len));
          ctx.params = ctx.params.slice(2);
          return [val, ctx];
        }
        
        const rem = ctx.storagePtr % 4;
        if (rem !== 0) { ctx.storagePtr += (4 - rem); }
        
        const dv = new DataView(ctx.memory.buffer);
        const start = dv.getUint32(ctx.storagePtr, true);
        const codeUnits = dv.getUint32(ctx.storagePtr + 4, true);
        
        val = TEXT_DECODER_UTF8.decode(new Uint8Array(ctx.memory.buffer, start, codeUnits));
        
        ctx.storagePtr += 8;
        if (ctx.storageLen !== undefined) { ctx.storagelen -= 8; }
        
        return [val, ctx];
      }
      
      function _liftFlatStringUTF16(ctx) {
        _debugLog('[_liftFlatStringUTF16()] args', { ctx });
        let val;
        
        if (ctx.useDirectParams) {
          if (ctx.params.length < 2) { throw new Error('expected at least two u32 arguments'); }
          const offset = ctx.params[0];
          if (!Number.isSafeInteger(offset)) {  throw new Error('invalid offset'); }
          const len = ctx.params[1];
          if (!Number.isSafeInteger(len)) {  throw new Error('invalid len'); }
          val = utf16Decoder.decode(new DataView(ctx.memory.buffer, offset, len));
          ctx.params = ctx.params.slice(2);
          return [val, ctx];
        }
        
        const data = new DataView(ctx.memory.buffer)
        const start = data.getUint32(ctx.storagePtr, vals[0], true);
        const codeUnits = data.getUint32(ctx.storagePtr, vals[0] + 4, true);
        val = utf16Decoder.decode(new Uint16Array(ctx.memory.buffer, start, codeUnits));
        ctx.storagePtr = ctx.storagePtr + 2 * codeUnits;
        if (ctx.storageLen !== undefined) { ctx.storageLen = ctx.storageLen - 2 * codeUnits }
        
        return [val, ctx];
      }
      
      function _liftFlatVariant(casesAndLiftFns) {
        return function _liftFlatVariantInner(ctx) {
          _debugLog('[_liftFlatVariant()] args', { ctx });
          
          const origUseParams = ctx.useDirectParams;
          
          let caseIdx;
          let liftRes;
          const originalPtr = ctx.storagePtr;
          const numCases =  casesAndLiftFns.length;
          if (casesAndLiftFns.length < 256) {
            liftRes = _liftFlatU8(ctx);
          } else if (numCases >= 256 && numCases < 65536) {
            liftRes = _liftFlatU16(ctx);
          } else if (numCases >= 65536 && numCases < 4_294_967_296) {
            liftRes = _liftFlatU32(ctx);
          } else {
            throw new Error(`unsupported number of variant cases [${numCases}]`);
          }
          caseIdx = liftRes[0];
          ctx = liftRes[1];
          
          const [ tag, liftFn, size32, align32, payloadOffset32, caseFlatCount, variantFlatCount ] = casesAndLiftFns[caseIdx];
          if (payloadOffset32 === undefined) { throw new Error('unexpectedly missing payload offset'); }
          
          if (originalPtr !== undefined) {
            ctx.storagePtr = originalPtr + payloadOffset32;
          }
          
          let val;
          if (liftFn === null) {
            val = { tag };
            // NOTE: here we need to move past the entire object in memory
            // despite moving to the payload which we now know is missing/unnecessary
            if (originalPtr !== undefined) {
              ctx.storagePtr = originalPtr + size32;
            }
          } else {
            const [newVal, newCtx] = liftFn(ctx);
            val = { tag, val: newVal };
            ctx = newCtx;
            
            // NOTE: Padding can be left over after doing the lift if it was less than
            // space left for the payload normally.
            if (originalPtr !== undefined) {
              ctx.storagePtr = Math.max(ctx.storagePtr, originalPtr + size32);
            }
          }
          
          if (origUseParams) {
            if (caseFlatCount === undefined || variantFlatCount === undefined) {
              throw new Error('variant flat count metadata is missing');
            }
            if (caseFlatCount === null || variantFlatCount === null) {
              throw new Error('cannot lift variant with unknown flat count');
            }
            const remainingPayloadParams = variantFlatCount - 1 - caseFlatCount;
            if (remainingPayloadParams < 0) {
              throw new Error(`invalid variant flat count metadata`);
            }
            if (ctx.params.length < remainingPayloadParams) {
              throw new Error(`expected at least [${remainingPayloadParams}] remaining variant payload params, but got [${ctx.params.length}]`);
            }
            ctx.params = ctx.params.slice(remainingPayloadParams);
          }
          
          if (ctx.storagePtr !== undefined) {
            const rem = ctx.storagePtr % align32;
            if (rem !== 0) { ctx.storagePtr += align32 - rem; }
          }
          
          return [val, ctx];
        }
      }
      
      function _liftFlatList(meta) {
        const { elemLiftFn, elemSize32, elemAlign32, knownLen, typedArray } = meta;
        
        const listValue =
        typedArray === undefined
        ? values => values
        : values => new typedArray(values);
        
        const readValuesAndReset = (ctx, originalPtr, dataPtr, len) => {
          ctx.storagePtr = dataPtr;
          const val = [];
          for (var i = 0; i < len; i++) {
            const elemPtr = dataPtr + i * elemSize32;
            ctx.storagePtr = elemPtr;
            const [res, nextCtx] = elemLiftFn(ctx);
            val.push(res);
            ctx = nextCtx;
            
            ctx.storagePtr = Math.max(ctx.storagePtr, elemPtr + elemSize32);
          }
          if (originalPtr !== null) { ctx.storagePtr = originalPtr; }
          return [listValue(val), ctx];
        };
        
        return function _liftFlatListInner(ctx) {
          _debugLog('[_liftFlatList()] args', { ctx });
          
          let liftResults;
          if (knownLen !== undefined) { // list with known length
          if (ctx.useDirectParams) {
            if (ctx.memory === null) {
              // If this lift should be using direct params,
              // and the memory is missing, we are in the case where
              // a fixed length list (or other value) is being passed only
              // via parameters to the function.
              //
              // Normally, we would expect to use the direct parameters as a
              // memory location + size, but in this case, *all* values are being passed directly,
              // via params.
              //
              _debugLog('memory unexpectedly missing while lifting unknown length list', { ctx });
              liftResults = [listValue(ctx.params.slice(0, knownLen)), ctx];
              ctx.params = ctx.params.slice(knownLen);
            } else {
              // in-memory list with unknown length w/ direct params
              const dataPtr = ctx.params[0];
              ctx.params = ctx.params.slice(1);
              
              ctx.useDirectParams = false;
              const originalPtr = ctx.storagePtr;
              ctx.storageLen = knownLen * elemSize32;
              
              liftResults = readValuesAndReset(ctx, originalPtr, dataPtr, knownLen);
              
              ctx.useDirectParams = true;
              ctx.storagePtr = undefined;
              ctx.storageLen = undefined;
            }
          } else { // indirect params
          if (ctx.memory === null) {
            _debugLog('memory unexpectedly missing while lifting known length list', { knownLen, ctx });
            throw new Error(`memory missing while lifting known length (${knownLen}) list`);
          }
          
          ctx.storageLen = knownLen * elemSize32;
          liftResults = readValuesAndReset(ctx, null, ctx.storagePtr, knownLen);
        }
        
      } else { // unknown length list
      
      if (ctx.useDirectParams) {
        // unknown length list ptr w/ direct params
        const dataPtr = ctx.params[0];
        const len = ctx.params[1];
        ctx.params = ctx.params.slice(2);
        
        ctx.useDirectParams = false;
        const originalPtr = ctx.storagePtr;
        ctx.storageLen = len * elemSize32;
        
        liftResults = readValuesAndReset(ctx, originalPtr, dataPtr, len);
        
        ctx.useDirectParams = true;
        ctx.storagePtr = undefined;
        ctx.storageLen = undefined;
        
      } else {
        // unknown length list ptr w/ in-memory params
        ctx.storageLen = 8;
        
        const dataPtrLiftRes = _liftFlatU32(ctx);
        const dataPtr = dataPtrLiftRes[0];
        ctx = dataPtrLiftRes[1];
        
        const lenLiftRes = _liftFlatU32(ctx);
        const len = lenLiftRes[0];
        ctx = lenLiftRes[1];
        
        const originalPtr = ctx.storagePtr;
        ctx.storagePtr = dataPtr;
        
        ctx.storageLen = len * elemSize32;
        liftResults = readValuesAndReset(ctx, originalPtr, dataPtr, len);
      }
    }
    
    return liftResults;
  }
}

function _liftFlatFlags(meta) {
  const { names, size32, align32, intSizeBytes } = meta;
  
  return function _liftFlatFlagsInner(ctx) {
    _debugLog('[_liftFlatFlags()] args', { ctx });
    
    const val = {};
    
    let liftRes;
    let align;
    switch (intSizeBytes) {
      case 1:
      liftRes = _liftFlatU8(ctx);
      break;
      case 2:
      liftRes = _liftFlatU16(ctx);
      break;
      case 4:
      liftRes = _liftFlatU32(ctx);
      break;
      default:
      throw new Error('invalid flags size');
    }
    let bits = liftRes[0];
    ctx = liftRes[1];
    
    for (const name of names) {
      val[name] = (bits & 1) === 1;
      bits >>>= 1;
    }
    
    const rem = ctx.storagePtr % align32;
    if (rem !== 0) { ctx.storagePtr += align32 - rem; }
    
    return [val, ctx];
  }
}

function _liftFlatResult(casesAndLiftFns) {
  return function _liftFlatResultInner(ctx) {
    _debugLog('[_liftFlatResult()] args', { ctx });
    return _liftFlatVariant(casesAndLiftFns)(ctx);
  }
}

function _liftFlatOwn(meta) {
  const { className, createResourceFn, componentIdx } = meta;
  
  return function _liftFlatOwnInner(ctx) {
    _debugLog('[_liftFlatOwn()] args', { ctx, className });
    
    if (ctx.componentIdx !== componentIdx) {
      throw new Error('invalid component for resource lift');
    }
    
    const [handle, newCtx] = _liftFlatU32(ctx);
    const resource = createResourceFn(handle);
    
    return [resource, newCtx];
  }
}

function _liftFlatBorrow(componentTableIdx, size, memory, vals, storagePtr, storageLen) {
  _debugLog('[_liftFlatBorrow()] args', { size, memory, vals, storagePtr, storageLen });
  throw new Error('flat lift for borrowed resources is not supported!');
}


function _lowerFlatU8(ctx) {
  _debugLog('[_lowerFlatU8()] args', ctx);
  
  if (ctx.vals.length !== 1) {
    throw new Error(`unexpected number [${ctx.vals.length}] of vals (expected 1)`);
  }
  
  _requireValidNumericPrimitive.bind('u8', ctx.vals[0]);
  
  if (!ctx.memory) { throw new Error("missing memory for lower"); }
  new DataView(ctx.memory.buffer).setUint32(ctx.storagePtr, ctx.vals[0], true);
  
  ctx.storagePtr += 1;
}

function _lowerFlatU16(ctx) {
  _debugLog('[_lowerFlatU16()] args', { ctx });
  
  if (!ctx.memory) { throw new Error("missing memory for lower"); }
  if (ctx.vals.length !== 1) {
    throw new Error(`unexpected number [${ctx.vals.length}] of vals (expected 1)`);
  }
  
  const rem = ctx.storagePtr % 2;
  if (rem !== 0) { ctx.storagePtr += (2 - rem); }
  
  _requireValidNumericPrimitive.bind('u16', ctx.vals[0]);
  new DataView(ctx.memory.buffer).setUint16(ctx.storagePtr, ctx.vals[0], true);
  
  ctx.storagePtr += 2;
}

function _lowerFlatU32(ctx) {
  _debugLog('[_lowerFlatU32()] args', { ctx });
  
  if (ctx.vals.length !== 1) {
    throw new Error(`expected single value to lower, got [${ctx.vals.length}]`);
  }
  
  const rem = ctx.storagePtr % 4;
  if (rem !== 0) { ctx.storagePtr += (4 - rem); }
  
  _requireValidNumericPrimitive.bind('u32', ctx.vals[0]);
  new DataView(ctx.memory.buffer).setUint32(ctx.storagePtr, ctx.vals[0], true);
  
  ctx.storagePtr += 4;
}

function _lowerFlatU64(ctx) {
  _debugLog('[_lowerFlatU64()] args', { ctx });
  
  if (ctx.vals.length !== 1) { throw new Error('unexpected number of vals'); }
  
  const rem = ctx.storagePtr % 8;
  if (rem !== 0) { ctx.storagePtr += (8 - rem); }
  
  _requireValidNumericPrimitive.bind('u64', ctx.vals[0]);
  new DataView(ctx.memory.buffer).setBigUint64(ctx.storagePtr, ctx.vals[0], true);
  
  ctx.storagePtr += 8;
}

function _lowerFlatStringAny(ctx) {
  switch (ctx.stringEncoding) {
    case 'utf8':
    return _lowerFlatStringUTF8(ctx);
    case 'utf16':
    return _lowerFlatStringUTF16(ctx);
    default:
    throw new Error(`missing/unrecognized/unsupported string encoding [${ctx.stringEncoding}]`);
  }
}

function _lowerFlatStringUTF8(ctx) {
  _debugLog('[_lowerFlatStringUTF8()] args', ctx);
  if (!ctx.realloc) { throw new Error('missing realloc during flat string lower'); }
  
  const s = ctx.vals[0];
  const { ptr, codepoints } = _utf8AllocateAndEncode(ctx.vals[0], ctx.realloc, ctx.memory);
  
  const view = new DataView(ctx.memory.buffer);
  view.setUint32(ctx.storagePtr, ptr, true);
  view.setUint32(ctx.storagePtr + 4, codepoints, true);
  
  ctx.storagePtr += 8;
}

function _lowerFlatStringUTF16(ctx) {
  _debugLog('[_lowerFlatStringUTF16()] args', { ctx });
  if (!ctx.realloc) { throw new Error('missing realloc during flat string lower'); }
  
  const s = ctx.vals[0];
  const { ptr, len, codepoints } = _utf16AllocateAndEncode(ctx.vals[0], ctx.realloc, ctx.memory);
  
  const view = new DataView(ctx.memory.buffer);
  view.setUint32(ctx.storagePtr, ptr, true);
  view.setUint32(ctx.storagePtr + 4, codepoints, true);
  
  const bytes = new Uint16Array(ctx.memory.buffer, start, codeUnits);
  if (ctx.memory.buffer.byteLength < start + bytes.byteLength) {
    throw new Error('memory out of bounds');
  }
  if (ctx.storageLen !== undefined && ctx.storageLen !== bytes.byteLength) {
    throw new Error(`storage length [${ctx.storageLen}] != [${bytes.byteLength}])`);
  }
  new Uint16Array(ctx.memory.buffer, ctx.storagePtr).set(bytes);
  
  ctx.storagePtr += len;
}

function _lowerFlatRecord(meta) {
  const { fieldMetas, size32: recordSize32, align32: recordAlign32 } = meta;
  return function _lowerFlatRecordInner(ctx) {
    _debugLog('[_lowerFlatRecord()] args', { ctx });
    
    const originalPtr = ctx.storagePtr;
    const r = ctx.vals[0];
    for (const [tag, lowerFn, size32, align32 ] of fieldMetas) {
      const rem = ctx.storagePtr % align32;
      if (rem !== 0) { ctx.storagePtr += align32 - rem; }
      
      const fieldPtr = ctx.storagePtr;
      ctx.vals = [r[tag]];
      lowerFn(ctx);
      
      ctx.storagePtr = Math.max(ctx.storagePtr, fieldPtr + size32);
    }
    
    ctx.storagePtr = Math.max(ctx.storagePtr, originalPtr + recordSize32);
    
    const rem = ctx.storagePtr % recordAlign32;
    if (rem !== 0) {
      ctx.storagePtr += recordAlign32 - rem;
    }
  }
}

function _lowerFlatVariant(lowerMetas) {
  let caseLookup = {};
  for (const [idx, meta] of lowerMetas.entries()) {
    let tag = meta[0];
    caseLookup[tag] = { discriminant: idx, meta };
  }
  
  return function _lowerFlatVariantInner(ctx) {
    _debugLog('[_lowerFlatVariant()] args', { ctx });
    
    const { tag, val } = ctx.vals[0];
    const variantCase = caseLookup[tag];
    if (!variantCase) {
      throw new Error(`missing tag [${tag}] (valid tags: ${Object.keys(caseLookup)})`);
    }
    
    const [ _tag, lowerFn, size32, align32, payloadOffset32 ] = variantCase.meta;
    
    const originalPtr = ctx.storagePtr;
    ctx.vals = [variantCase.discriminant];
    let discLowerRes;
    if (lowerMetas.length < 256) {
      discLowerRes = _lowerFlatU8(ctx);
    } else if (lowerMetas.length >= 256 && lowerMetas.length < 65536) {
      discLowerRes = _lowerFlatU16(ctx);
    } else if (lowerMetas.length >= 65536 && lowerMetas.length < 4_294_967_296) {
      discLowerRes = _lowerFlatU32(ctx);
    } else {
      throw new Error(`unsupported number of cases [${lowerMetas.length}]`);
    }
    
    const payloadOffsetPtr = originalPtr + payloadOffset32;
    ctx.storagePtr = payloadOffsetPtr;
    ctx.vals = [val];
    if (lowerFn) { lowerFn(ctx); }
    
    ctx.storagePtr = Math.max(ctx.storagePtr, originalPtr + size32);
    
    const rem = ctx.storagePtr % align32;
    if (rem !== 0) { ctx.storagePtr += align32 - rem; }
  }
}

function _lowerFlatList(meta) {
  const {
    elemLowerFn,
    knownLen,
    size32,
    align32,
    elemSize32,
    elemAlign32,
  } = meta;
  
  if (!elemLowerFn) { throw new TypeError("missing/invalid element lower fn for list"); }
  
  return function _lowerFlatListInner(ctx) {
    _debugLog('[_lowerFlatList()] args', { ctx });
    
    if (ctx.useDirectParams) {
      if (ctx.params.length < 2) { throw new Error('insufficient params left to lower list'); }
      const storagePtr = ctx.params[0];
      const elemCount = ctx.params[1];
      ctx.params = ctx.params.slice(2);
      
      const list = ctx.vals[0];
      if (!list) { throw new Error("missing direct param value"); }
      
      const lowerCtx = {
        storagePtr,
        memory: ctx.memory,
        stringEncoding: ctx.stringEncoding,
      };
      for (let idx = 0; idx < list.length; idx++) {
        const elemPtr = storagePtr + idx * elemSize32;
        lowerCtx.storagePtr = elemPtr;
        lowerCtx.vals = list.slice(idx, idx+1);
        elemLowerFn(lowerCtx);
        lowerCtx.storagePtr = Math.max(lowerCtx.storagePtr, elemPtr + elemSize32);
      }
      ctx.storagePtr = lowerCtx.storagePtr;
      
      // TODO: implement parma-only known-length processing
      
      return;
    }
    
    // TODO(fix): is it possible to get a vals that are a addr and length here from
    // a component lower?
    
    const elems = ctx.vals[0];
    if (knownLen === undefined) {
      // unknown length
      if (!ctx.realloc) { throw new Error('missing realloc during flat string lower'); }
      const dataPtr = ctx.realloc(0, 0, elemAlign32, elemSize32 * elems.length);
      
      ctx.vals[0] = dataPtr;
      _lowerFlatU32(ctx);
      
      ctx.vals[0] = elems.length;
      _lowerFlatU32(ctx);
      
      const origPtr = ctx.storagePtr;
      ctx.storagePtr = dataPtr;
      
      for (const [idx, elem] of elems.entries()) {
        const elemPtr = dataPtr + idx * elemSize32;
        ctx.storagePtr = elemPtr;
        ctx.vals = [elem];
        elemLowerFn(ctx);
        ctx.storagePtr = Math.max(ctx.storagePtr, elemPtr + elemSize32);
      }
      
      ctx.storagePtr = origPtr;
      
    } else {
      // known length
      
      if (elems.length !== knownLen) {
        throw new TypeError(`invalid list input of length [${elems.length}], must be length [${knownLen}]`);
      }
      
      const originalPtr = ctx.storagePtr;
      for (const [idx, elem] of elems.entries()) {
        const elemPtr = originalPtr + idx * elemSize32;
        ctx.storagePtr = elemPtr;
        ctx.vals = [elem];
        elemLowerFn(ctx);
        ctx.storagePtr = Math.max(ctx.storagePtr, elemPtr + elemSize32);
      }
    }
    
    // TODO(fix): special case for u8/u16/etc, we can do a direct copy
    
    const totalSizeBytes = elems.length * size32;
    if (ctx.storageLen !== undefined && totalSizeBytes > ctx.storageLen) {
      throw new Error('not enough storage remaining for list flat lower');
    }
  }
}

function _lowerFlatTuple(meta) {
  const { elemLowerMetas, size32: tupleSize32, align32: tupleAlign32 } = meta;
  return function _lowerFlatTupleInner(ctx) {
    _debugLog('[_lowerFlatTuple()] args', { ctx });
    const originalPtr = ctx.storagePtr;
    const tuple = ctx.vals[0];
    for (const [idx, [ lowerFn, size32, align32 ]]  of elemLowerMetas.entries()) {
      const rem = ctx.storagePtr % align32;
      if (rem !== 0) { ctx.storagePtr += align32 - rem; }
      
      const elemPtr = ctx.storagePtr;
      ctx.vals = [tuple[idx]];
      lowerFn(ctx);
      ctx.storagePtr = Math.max(ctx.storagePtr, elemPtr + size32);
    }
    
    ctx.storagePtr = Math.max(ctx.storagePtr, originalPtr + tupleSize32);
    
    const rem = ctx.storagePtr % tupleAlign32;
    if (rem !== 0) {
      ctx.storagePtr += tupleAlign32 - rem;
    }
  }
}

function _lowerFlatResult(lowerMetas) {
  return function _lowerFlatResultInner(ctx) {
    _debugLog('[_lowerFlatResult()] args', { lowerMetas });
    
    const v = ctx.vals[0];
    const isNotResultObject = typeof v !== 'object'
    || Object.keys(v).length !== 2
    || !('tag' in v)
    || !('ok' === v.tag || 'err' === v.tag)
    || !('val' in v);
    if (isNotResultObject) {
      ctx.vals[0] = { tag: 'ok', val: v };
    }
    
    _lowerFlatVariant(lowerMetas)(ctx);
  };
}

function _lowerFlatOwn(meta) {
  const { lowerFn, componentIdx } = meta;
  
  return function _lowerFlatOwnInner(ctx) {
    _debugLog('[_lowerFlatOwn()] args', { ctx });
    const { createFn } = ctx;
    
    if (ctx.componentIdx !== componentIdx) {
      throw new Error(`component index mismatch (expected [${componentIdx}], lift called from [${ctx.componentIdx}])`);
    }
    
    const obj = ctx.vals[0];
    if (obj === undefined || obj === null) { throw new Error('missing resource'); }
    const handle = lowerFn(obj);
    
    ctx.vals[0] = handle;
    _lowerFlatU32(ctx);
  };
}

const STREAMS = new RepTable({ target: 'global stream map' });
const ASYNC_STATE = new Map();

function getOrCreateAsyncState(componentIdx, init) {
  if (!ASYNC_STATE.has(componentIdx)) {
    const newState = new ComponentAsyncState({ componentIdx });
    ASYNC_STATE.set(componentIdx, newState);
  }
  return ASYNC_STATE.get(componentIdx);
}

class ComponentAsyncState {
  static EVENT_HANDLER_EVENTS = [ 'backpressure-change' ];
  
  #componentIdx;
  #callingAsyncImport = false;
  #syncImportWait = promiseWithResolvers();
  #locked = false;
  #parkedTasks = new Map();
  #suspendedTasksByTaskID = new Map();
  #suspendedTaskIDs = [];
  #errored = null;
  
  #backpressure = 0;
  #backpressureWaiters = 0n;
  
  #handlerMap = new Map();
  #nextHandlerID = 0n;
  
  #tickLoop = null;
  #tickLoopInterval = null;
  
  #onExclusiveReleaseHandlers = [];
  
  mayLeave = true;
  
  handles;
  subtasks;
  
  constructor(args) {
    this.#componentIdx = args.componentIdx;
    this.handles = new RepTable({ target: `component [${this.#componentIdx}] handles (waitable objects)` });
    this.subtasks = new RepTable({ target: `component [${this.#componentIdx}] subtasks` });
  };
  
  componentIdx() { return this.#componentIdx; }
  
  errored() { return this.#errored !== null; }
  setErrored(err) {
    _debugLog('[ComponentAsyncState#setErrored()] component errored', { err, componentIdx: this.#componentIdx });
    if (this.#errored) { return; }
    if (!err) {
      err = new Error('error elswehere (see other component instance error)')
      err.componentIdx = this.#componentIdx;
    }
    this.#errored = err;
  }
  
  callingSyncImport(val) {
    if (val === undefined) { return this.#callingAsyncImport; }
    if (typeof val !== 'boolean') { throw new TypeError('invalid setting for async import'); }
    const prev = this.#callingAsyncImport;
    this.#callingAsyncImport = val;
    if (prev === true && this.#callingAsyncImport === false) {
      this.#notifySyncImportEnd();
    }
  }
  
  #notifySyncImportEnd() {
    const existing = this.#syncImportWait;
    this.#syncImportWait = promiseWithResolvers();
    existing.resolve();
  }
  
  async waitForSyncImportCallEnd() {
    await this.#syncImportWait.promise;
  }
  
  setBackpressure(v) {
    this.#backpressure = v;
    return this.#backpressure
  }
  getBackpressure() { return this.#backpressure; }
  
  incrementBackpressure() {
    const current = this.#backpressure;
    if (current < 0 || current > 2**16) {
      throw new Error(`invalid current backpressure value [${current}]`);
    }
    const newValue = this.getBackpressure() + 1;
    if (newValue >= 2**16) {
      throw new Error(`invalid new backpressure value [${newValue}], overflow`);
    }
    return this.setBackpressure(newValue);
  }
  
  decrementBackpressure() {
    const current = this.#backpressure;
    if (current < 0 || current > 2**16) {
      throw new Error(`invalid current backpressure value [${current}]`);
    }
    const newValue = Math.max(0, current - 1);
    if (newValue < 0) {
      throw new Error(`invalid new backpressure value [${newValue}], underflow`);
    }
    return this.setBackpressure(newValue);
  }
  hasBackpressure() { return this.#backpressure > 0; }
  
  waitForBackpressure() {
    let backpressureCleared = false;
    const cstate = this;
    cstate.addBackpressureWaiter();
    const handlerID = this.registerHandler({
      event: 'backpressure-change',
      fn: (bp) => {
        if (bp === 0) {
          cstate.removeHandler(handlerID);
          backpressureCleared = true;
        }
      }
    });
    return new Promise((resolve) => {
      const interval = setInterval(() => {
        if (backpressureCleared) { return; }
        clearInterval(interval);
        cstate.removeBackpressureWaiter();
        resolve(null);
      }, 0);
    });
  }
  
  registerHandler(args) {
    const { event, fn } = args;
    if (!event) { throw new Error("missing handler event"); }
    if (!fn) { throw new Error("missing handler fn"); }
    
    if (!ComponentAsyncState.EVENT_HANDLER_EVENTS.includes(event)) {
      throw new Error(`unrecognized event handler [${event}]`);
    }
    
    const handlerID = this.#nextHandlerID++;
    let handlers = this.#handlerMap.get(event);
    if (!handlers) {
      handlers = [];
      this.#handlerMap.set(event, handlers)
    }
    
    handlers.push({ id: handlerID, fn, event });
    return handlerID;
  }
  
  removeHandler(args) {
    const { event, handlerID } = args;
    const registeredHandlers = this.#handlerMap.get(event);
    if (!registeredHandlers) { return; }
    const found = registeredHandlers.find(h => h.id === handlerID);
    if (!found) { return; }
    this.#handlerMap.set(event, this.#handlerMap.get(event).filter(h => h.id !== handlerID));
  }
  
  getBackpressureWaiters() { return this.#backpressureWaiters; }
  addBackpressureWaiter() { this.#backpressureWaiters++; }
  removeBackpressureWaiter() {
    this.#backpressureWaiters--;
    if (this.#backpressureWaiters < 0) {
      throw new Error("unexepctedly negative number of backpressure waiters");
    }
  }
  
  isExclusivelyLocked() { return this.#locked === true; }
  setLocked(locked) {
    this.#locked = locked;
  }
  
  exclusiveLock() {
    _debugLog('[ComponentAsyncState#exclusiveLock()]', {
      locked: this.#locked,
      componentIdx: this.#componentIdx,
    });
    this.setLocked(true);
  }
  
  exclusiveRelease() {
    _debugLog('[ComponentAsyncState#exclusiveRelease()] args', {
      locked: this.#locked,
      componentIdx: this.#componentIdx,
    });
    this.setLocked(false);
    
    this.#onExclusiveReleaseHandlers = this.#onExclusiveReleaseHandlers.filter(v => !!v);
    for (const [idx, f] of this.#onExclusiveReleaseHandlers.entries()) {
      try {
        this.#onExclusiveReleaseHandlers[idx] = null;
        f();
      } catch (err) {
        _debugLog("error while executing handler for next exclusive release", err);
        throw err;
      }
    }
  }
  
  onNextExclusiveRelease(fn) {
    _debugLog('[ComponentAsyncState#()onNextExclusiveRelease] registering');
    this.#onExclusiveReleaseHandlers.push(fn);
  }
  
  // nextTaskPromise & nextTaskQueue are used to await current task completion and queues
  // any tasks attempting to enter() and complete.
  //
  // see: nextTaskExecutionSlot()
  //
  // TODO(threads): this should be unnecessary once threads are properly implemented,
  // as the task.enter() logic should suffice (it should be guaranteed that we cannot re-enter
  // unless the task in question is the current task in the thread execution, and only one can
  // run at a time)
  #nextTaskPromise = Promise.resolve(true);
  #nextTaskQueue = [];
  
  async nextTaskExecutionSlot(args) {
    const { task } = args;
    
    const placeholder = {
      completed: false,
      task,
      promise: task.exitPromise().then(() => {
        placeholder.completed = true;
      }),
    };
    this.#nextTaskQueue.push(placeholder);
    
    let next;
    while (true) {
      await this.#nextTaskPromise;
      
      next = this.#nextTaskQueue.find(placeholder => !placeholder.completed);
      
      // This task is next in the queue, we can continue
      if (next === undefined || next === placeholder) {
        this.#nextTaskPromise = next.promise;
        if (this.#nextTaskQueue.length > 1000) {
          this.#nextTaskQueue = this.#nextTaskQueue.filter(p => !p.completed);
          if (this.#nextTaskQueue.length > 1000) {
            _debugLog('[ComponentAsyncState#()nextTaskExecutionSlot] next task queue length > 1000 even after cleanup, tasks may be leaking');
          }
        }
        break;
      }
      
      // If we get here, this task was *not* next in the queue, continue waiting
      // (at this point the task that *is* next will likely have already set itself
      // as this.#nextTaskPromise)
    }
  }
  
  #getSuspendedTaskMeta(taskID) {
    return this.#suspendedTasksByTaskID.get(taskID);
  }
  
  #removeSuspendedTaskMeta(taskID) {
    _debugLog('[ComponentAsyncState#removeSuspendedTaskMeta()] removing suspended task', { taskID });
    const idx = this.#suspendedTaskIDs.findIndex(t => t === taskID);
    const meta = this.#suspendedTasksByTaskID.get(taskID);
    this.#suspendedTaskIDs[idx] = null;
    this.#suspendedTasksByTaskID.delete(taskID);
    return meta;
  }
  
  #addSuspendedTaskMeta(meta) {
    if (!meta) { throw new Error('missing task meta'); }
    const taskID = meta.taskID;
    this.#suspendedTasksByTaskID.set(taskID, meta);
    this.#suspendedTaskIDs.push(taskID);
    if (this.#suspendedTasksByTaskID.size < this.#suspendedTaskIDs.length - 10) {
      this.#suspendedTaskIDs = this.#suspendedTaskIDs.filter(t => t !== null);
    }
  }
  
  // TODO(threads): readyFn is normally on the thread
  suspendTask(args) {
    const { task, readyFn } = args;
    const taskID = task.id();
    _debugLog('[ComponentAsyncState#suspendTask()]', {
      taskID,
      componentIdx: this.#componentIdx,
      taskEntryFnName: task.entryFnName(),
      subtask: task.getParentSubtask(),
    });
    
    if (this.#getSuspendedTaskMeta(taskID)) {
      throw new Error(`task [${taskID}] already suspended`);
    }
    
    const { promise, resolve, reject } = promiseWithResolvers();
    this.#addSuspendedTaskMeta({
      task,
      taskID,
      readyFn,
      resume: () => {
        _debugLog('[ComponentAsyncState#suspendTask()] resuming suspended task', { taskID });
        // TODO(threads): it's thread cancellation we should be checking for below, not task
        resolve(!task.isCancelled());
      },
    });
    
    this.runTickLoop();
    
    return promise;
  }
  
  resumeTaskByID(taskID) {
    const meta = this.#removeSuspendedTaskMeta(taskID);
    if (!meta) { return; }
    if (meta.taskID !== taskID) { throw new Error('task ID does not match'); }
    meta.resume();
  }
  
  async runTickLoop() {
    if (this.#tickLoop !== null) { return; }
    this.#tickLoop = 1;
    setTimeout(async () => {
      let done = this.tick();
      while (!done) {
        await new Promise((resolve) => setTimeout(resolve, 30));
        done = this.tick();
      }
      this.#tickLoop = null;
    }, 10);
  }
  
  tick() {
    // _debugLog('[ComponentAsyncState#tick()]', { suspendedTaskIDs: this.#suspendedTaskIDs });
    
    const resumableTasks = this.#suspendedTaskIDs.filter(t => t !== null);
    for (const taskID of resumableTasks) {
      const meta = this.#suspendedTasksByTaskID.get(taskID);
      if (!meta || !meta.readyFn) {
        throw new Error(`missing/invalid task despite ID [${taskID}] being present`);
      }
      
      // If the task failed via any means, allow the task to resume because
      // it's been cancelled -- the callback should immediately exit as well
      if (meta.task.isRejected()) {
        _debugLog('[ComponentAsyncState#suspendTask()] detected task rejection, leaving early', { meta });
        this.resumeTaskByID(taskID);
        return;
      }
      
      const isReady = meta.readyFn();
      if (!isReady) { continue; }
      
      this.resumeTaskByID(taskID);
    }
    
    return this.#suspendedTaskIDs.filter(t => t !== null).length === 0;
  }
  
  addStreamEndToTable(args) {
    _debugLog('[ComponentAsyncState#addStreamEnd()] args', args);
    const { tableIdx, streamEnd } = args;
    if (typeof streamEnd === 'number') { throw new Error("INSERTING BAD STREAMEND"); }
    
    let { table, componentIdx } = STREAM_TABLES[tableIdx];
    if (componentIdx === undefined || !table) {
      throw new Error(`invalid global stream table state for table [${tableIdx}]`);
    }
    
    const handle = table.insert(streamEnd);
    streamEnd.setHandle(handle);
    streamEnd.setStreamTableIdx(tableIdx);
    
    const cstate = getOrCreateAsyncState(componentIdx);
    const waitableIdx = cstate.handles.insert(streamEnd);
    streamEnd.setWaitableIdx(waitableIdx);
    
    _debugLog('[ComponentAsyncState#addStreamEnd()] added stream end', {
      tableIdx,
      table,
      handle,
      streamEnd,
      destComponentIdx: componentIdx,
    });
    
    return { handle, waitableIdx };
  }
  
  createWaitable(args) {
    return new Waitable({ target: args?.target, });
  }
  
  createReadableStreamEnd(args) {
    _debugLog('[ComponentAsyncState#createStreamEnd()] args', args);
    const { tableIdx, elemMeta, hostInjectFn } = args;
    
    const { table: localStreamTable, componentIdx } = STREAM_TABLES[tableIdx];
    if (!localStreamTable) {
      throw new Error(`missing global stream table lookup for table [${tableIdx}] while creating stream`);
    }
    if (componentIdx !== this.#componentIdx) {
      throw new Error('component idx mismatch while creating stream');
    }
    
    const waitable = this.createWaitable();
    const streamEnd = new StreamReadableEnd({
      tableIdx,
      elemMeta,
      hostInjectFn,
      pendingBufferMeta: {},
      target: `stream read end (lowered, @init)`,
      waitable,
    });
    
    streamEnd.setWaitableIdx(this.handles.insert(streamEnd));
    streamEnd.setHandle(localStreamTable.insert(streamEnd));
    if (streamEnd.streamTableIdx() !== tableIdx) {
      throw new Error("unexpectedly mismatched stream table");
    }
    const streamEndWaitableIdx = streamEnd.waitableIdx();
    const streamEndHandle = streamEnd.handle();
    waitable.setTarget(`waitable for stream read end (lowered, waitable [${streamEndWaitableIdx}])`);
    streamEnd.setTarget(`stream read end (lowered, waitable [${streamEndWaitableIdx}])`);
    
    return {
      waitableIdx: streamEndWaitableIdx,
      handle: streamEndHandle,
      streamEnd,
    };
  }
  
  createStream(args) {
    _debugLog('[ComponentAsyncState#createStream()] args', args);
    const { tableIdx, elemMeta, hostInjectFn } = args;
    if (tableIdx === undefined) { throw new Error("missing table idx while adding stream"); }
    if (elemMeta === undefined) { throw new Error("missing element metadata while adding stream"); }
    
    const { table: localStreamTable, componentIdx } = STREAM_TABLES[tableIdx];
    if (!localStreamTable) {
      throw new Error(`missing global stream table lookup for table [${tableIdx}] while creating stream`);
    }
    if (componentIdx !== this.#componentIdx) {
      throw new Error('component idx mismatch while creating stream');
    }
    
    const readWaitable = this.createWaitable();
    const writeWaitable = this.createWaitable();
    
    const stream = new InternalStream({
      tableIdx,
      elemMeta,
      readWaitable,
      writeWaitable,
      hostInjectFn,
    });
    stream.setGlobalStreamMapRep(STREAMS.insert(stream));
    
    const writeEnd = stream.writeEnd();
    writeEnd.setWaitableIdx(this.handles.insert(writeEnd));
    writeEnd.setHandle(localStreamTable.insert(writeEnd));
    if (writeEnd.streamTableIdx() !== tableIdx) { throw new Error("unexpectedly mismatched stream table"); }
    
    const writeEndWaitableIdx = writeEnd.waitableIdx();
    const writeEndHandle = writeEnd.handle();
    writeWaitable.setTarget(`waitable for stream write end (waitable [${writeEndWaitableIdx}])`);
    writeEnd.setTarget(`stream write end (waitable [${writeEndWaitableIdx}])`);
    
    const readEnd = stream.readEnd();
    readEnd.setWaitableIdx(this.handles.insert(readEnd));
    readEnd.setHandle(localStreamTable.insert(readEnd));
    if (readEnd.streamTableIdx() !== tableIdx) { throw new Error("unexpectedly mismatched stream table"); }
    
    const readEndWaitableIdx = readEnd.waitableIdx();
    const readEndHandle = readEnd.handle();
    readWaitable.setTarget(`waitable for read end (waitable [${readEndWaitableIdx}])`);
    readEnd.setTarget(`stream read end (waitable [${readEndWaitableIdx}])`);
    
    return {
      writeEnd,
      writeEndWaitableIdx,
      writeEndHandle,
      readEndWaitableIdx,
      readEndHandle,
      readEnd,
    };
  }
  
  getStreamEnd(args) {
    _debugLog('[ComponentAsyncState#getStreamEnd()] args', args);
    const { tableIdx, streamEndHandle, streamEndWaitableIdx } = args;
    if (tableIdx === undefined) {
      throw new Error('missing table idx while getting stream end');
    }
    
    const { table, componentIdx } = STREAM_TABLES[tableIdx];
    const cstate = getOrCreateAsyncState(componentIdx);
    
    let streamEnd;
    if (streamEndWaitableIdx !== undefined) {
      streamEnd = cstate.handles.get(streamEndWaitableIdx);
    } else if (streamEndHandle !== undefined) {
      if (!table) { throw new Error(`missing/invalid table [${tableIdx}] while getting stream end`); }
      streamEnd = table.get(streamEndHandle);
    } else {
      throw new TypeError("must specify either waitable idx or handle to retrieve stream");
    }
    
    if (!streamEnd) {
      throw new Error(`missing stream end (tableIdx [${tableIdx}], handle [${streamEndHandle}], waitableIdx [${streamEndWaitableIdx}])`);
    }
    if (tableIdx && streamEnd.streamTableIdx() !== tableIdx) {
      throw new Error(`stream end table idx [${streamEnd.streamTableIdx()}] does not match [${tableIdx}]`);
    }
    
    return streamEnd;
  }
  
  deleteStreamEnd(args) {
    _debugLog('[ComponentAsyncState#deleteStreamEnd()] args', args);
    const { tableIdx, streamEndWaitableIdx } = args;
    if (tableIdx === undefined) { throw new Error("missing table idx while removing stream end"); }
    if (streamEndWaitableIdx === undefined) { throw new Error("missing stream idx while removing stream end"); }
    
    const { table, componentIdx } = STREAM_TABLES[tableIdx];
    const cstate = getOrCreateAsyncState(componentIdx);
    
    const streamEnd = cstate.handles.get(streamEndWaitableIdx);
    if (!streamEnd) {
      throw new Error(`missing stream end [${streamEndWaitableIdx}] in component handles while deleting stream`);
    }
    if (streamEnd.streamTableIdx() !== tableIdx) {
      throw new Error(`stream end table idx [${streamEnd.streamTableIdx()}] does not match [${tableIdx}]`);
    }
    
    let removed = cstate.handles.remove(streamEnd.waitableIdx());
    if (!removed) {
      throw new Error(`failed to remove stream end [${streamEndWaitableIdx}] waitable obj in component [${componentIdx}]`);
    }
    
    removed = table.remove(streamEnd.handle());
    if (!removed) {
      throw new Error(`failed to remove stream end with handle [${streamEnd.handle()}] from stream table [${tableIdx}] in component [${componentIdx}]`);
    }
    
    return streamEnd;
  }
  
  removeStreamEndFromTable(args) {
    _debugLog('[ComponentAsyncState#removeStreamEndFromTable()] args', args);
    
    const { tableIdx, streamWaitableIdx } = args;
    if (tableIdx === undefined) { throw new Error("missing table idx while removing stream end"); }
    if (streamWaitableIdx === undefined) {
      throw new Error("missing stream end waitable idx while removing stream end");
    }
    
    const { table, componentIdx } = STREAM_TABLES[tableIdx];
    if (!table) { throw new Error(`missing/invalid table [${tableIdx}] while removing stream end`); }
    
    const cstate = getOrCreateAsyncState(componentIdx);
    
    const streamEnd = cstate.handles.get(streamWaitableIdx);
    if (!streamEnd) {
      throw new Error(`missing stream end (handle [${streamWaitableIdx}], table [${tableIdx}])`);
    }
    const handle = streamEnd.handle();
    
    let removed = cstate.handles.remove(streamWaitableIdx);
    if (!removed) {
      throw new Error(`failed to remove streamEnd from handles (waitable idx [${streamWaitableIdx}]), component [${componentIdx}])`);
    }
    
    removed = table.remove(handle);
    if (!removed) {
      throw new Error(`failed to remove streamEnd from table (handle [${handle}]), table [${tableIdx}], component [${componentIdx}])`);
    }
    
    return streamEnd;
  }
  
  createFuture(args) {
    _debugLog('[ComponentAsyncState#createFuture()] args', args);
    const { tableIdx, elemMeta, hostInjectFn } = args;
    if (tableIdx === undefined) { throw new Error("missing table idx while adding future"); }
    if (elemMeta === undefined) { throw new Error("missing element metadata while adding future"); }
    
    const { table: futureTable, componentIdx } = FUTURE_TABLES[tableIdx];
    if (!futureTable) {
      throw new Error(`missing global future table lookup for table [${tableIdx}] while creating future`);
    }
    if (componentIdx !== this.#componentIdx) {
      throw new Error('component idx mismatch while creating future');
    }
    
    const readWaitable = this.createWaitable();
    const writeWaitable = this.createWaitable();
    
    const future = new InternalFuture({
      tableIdx,
      componentIdx: this.#componentIdx,
      elemMeta,
      readWaitable,
      writeWaitable,
      hostInjectFn,
    });
    future.setGlobalFutureMapRep(FUTURES.insert(future));
    
    const writeEnd = future.writeEnd();
    writeEnd.setWaitableIdx(this.handles.insert(writeEnd));
    writeEnd.setHandle(futureTable.insert(writeEnd));
    if (writeEnd.futureTableIdx() !== tableIdx) { throw new Error("unexpectedly mismatched future table"); }
    
    const writeEndWaitableIdx = writeEnd.waitableIdx();
    const writeEndHandle = writeEnd.handle();
    writeWaitable.setTarget(`waitable for future write end (waitable [${writeEndWaitableIdx}])`);
    writeEnd.setTarget(`future write end (waitable [${writeEndWaitableIdx}])`);
    
    const readEnd = future.readEnd();
    readEnd.setWaitableIdx(this.handles.insert(readEnd));
    readEnd.setHandle(futureTable.insert(readEnd));
    if (readEnd.futureTableIdx() !== tableIdx) { throw new Error("unexpectedly mismatched future table"); }
    
    const readEndWaitableIdx = readEnd.waitableIdx();
    const readEndHandle = readEnd.handle();
    readWaitable.setTarget(`waitable for read end (waitable [${readEndWaitableIdx}])`);
    readEnd.setTarget(`future read end (waitable [${readEndWaitableIdx}])`);
    
    return {
      writeEnd,
      writeEndWaitableIdx,
      writeEndHandle,
      readEndWaitableIdx,
      readEndHandle,
      readEnd,
    };
  }
  
  getFutureEnd(args) {
    _debugLog('[ComponentAsyncState#getFutureEnd()] args', args);
    const { tableIdx, futureEndHandle, futureEndWaitableIdx } = args;
    if (tableIdx === undefined) {
      throw new Error('missing table idx while getting future end');
    }
    
    const { table, componentIdx } = FUTURE_TABLES[tableIdx];
    const cstate = getOrCreateAsyncState(componentIdx);
    
    let futureEnd;
    if (futureEndWaitableIdx !== undefined) {
      futureEnd = cstate.handles.get(futureEndWaitableIdx);
    } else if (futureEndHandle !== undefined) {
      if (!table) { throw new Error(`missing/invalid table [${tableIdx}] while getting future end`); }
      futureEnd = table.get(futureEndHandle);
    } else {
      throw new TypeError("must specify either waitable idx or handle to retrieve future");
    }
    
    if (!futureEnd) {
      throw new Error(`missing future end (tableIdx [${tableIdx}], handle [${futureEndHandle}], waitableIdx [${futureEndWaitableIdx}])`);
    }
    if (tableIdx && futureEnd.futureTableIdx() !== tableIdx) {
      throw new Error(`future end table idx [${futureEnd.futureTableIdx()}] does not match [${tableIdx}]`);
    }
    
    return futureEnd;
  }
  
  removeFutureEndFromTable(args) {
    _debugLog('[ComponentAsyncState#removeFutureEndFromTable()] args', args);
    
    const { tableIdx, futureWaitableIdx } = args;
    if (tableIdx === undefined) { throw new Error("missing table idx while removing future end"); }
    if (futureWaitableIdx === undefined) {
      throw new Error("missing future end waitable idx while removing future end");
    }
    
    const { table, componentIdx } = FUTURE_TABLES[tableIdx];
    if (!table) { throw new Error(`missing/invalid table [${tableIdx}] while removing future end`); }
    
    const cstate = getOrCreateAsyncState(componentIdx);
    
    const futureEnd = cstate.handles.get(futureWaitableIdx);
    if (!futureEnd) {
      throw new Error(`missing future end (handle [${futureWaitableIdx}], table [${tableIdx}])`);
    }
    const handle = futureEnd.handle();
    
    let removed = cstate.handles.remove(futureWaitableIdx);
    if (!removed) {
      throw new Error(`failed to remove futureEnd from handles (waitable idx [${futureWaitableIdx}]), component [${componentIdx}])`);
    }
    
    removed = table.remove(handle);
    if (!removed) {
      throw new Error(`failed to remove futureEnd from table (handle [${handle}]), table [${tableIdx}], component [${componentIdx}])`);
    }
    
    return futureEnd;
  }
  
}

function _ComponentStateSetAllError() {
  _debugLog('[_ComponentStateSetAllError()]');
  for (const state of ASYNC_STATE.values()) {
    state.setErrored();
  }
}

function _storeEventInComponentMemory(args) {
  _debugLog('[_storeEventInComponentMemory()] args', args);
  const { memory, ptr, event } = args;
  
  if (!memory) { throw new Error('unexpectedly missing memory'); }
  if (ptr === undefined || ptr === null) { throw new Error('unexpectedly missing pointer'); }
  if (!event) { throw new Error('event object missing'); }
  if (event.code === undefined) { throw new Error('invalid event object, missing code'); }
  if (event.payload0 === undefined) { throw new Error('invalid event object, missing payload0'); }
  if (event.payload1 === undefined) { throw new Error('invalid event object, missing payload1'); }
  
  const dv = new DataView(memory.buffer);
  dv.setUint32(ptr, event.payload0, true);
  dv.setUint32(ptr + 4, event.payload1, true);
  
  return event.code;
}

const isNode = typeof process !== 'undefined' && process.versions && process.versions.node;
let _fs;
async function fetchCompile (url) {
  if (isNode) {
    _fs = _fs || await import('node:fs/promises');
    return WebAssembly.compile(await _fs.readFile(url));
  }
  return fetch(url).then(WebAssembly.compileStreaming);
}

const symbolCabiDispose = Symbol.for('cabiDispose');

const symbolRscHandle = Symbol('handle');

const symbolRscRep = Symbol.for('cabiRep');

const handleTables = [];

function getErrorPayload(e) {
  if (e && hasOwnProperty.call(e, 'payload')) return e.payload;
  if (e instanceof Error) throw e;
  return e;
}

const ASYNC_EVENT_CODE = {
  NONE: 0,
  SUBTASK: 1,
  STREAM_READ: 2,
  STREAM_WRITE: 3,
  FUTURE_READ: 4,
  FUTURE_WRITE: 5,
  TASK_CANCELLED: 6,
};

const isLE = new Uint8Array(new Uint16Array([1]).buffer)[0] === 1;

const hasOwnProperty = Object.prototype.hasOwnProperty;


if (!getCoreModule) getCoreModule = (name) => fetchCompile(new URL(`./${name}`, import.meta.url));
const module0 = getCoreModule('readwrite.core.wasm');
const module1 = getCoreModule('readwrite.core2.wasm');
const module2 = getCoreModule('readwrite.core3.wasm');

const { File, default: _default, open, read, write } = imports['eo9:fs/fs'];

if (File=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'File', was 'File' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

File._isHostProvided = true;

if (_default=== undefined) {
  const err = new Error("unexpectedly undefined instance import '_default', was 'default' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

_default._isHostProvided = true;

if (open=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'open', was 'open' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

open._isHostProvided = true;

if (read=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'read', was 'read' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

read._isHostProvided = true;

if (write=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'write', was 'write' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

write._isHostProvided = true;
const { FsImpl } = imports['eo9:fs/types'];

if (FsImpl=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'FsImpl', was 'FsImpl' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

FsImpl._isHostProvided = true;
const { Buffer } = imports['eo9:io/buffers'];

if (Buffer=== undefined) {
  const err = new Error("unexpectedly undefined instance import 'Buffer', was 'Buffer' available at instantiation?");
  console.error("ERROR:", err.toString());
  throw err;
}

Buffer._isHostProvided = true;
let gen = (function* _initGenerator () {
  let exports0;
  const handleTable0 = [T_FLAG, 0];
  const captureTable0= new Map();
  let captureCnt0 = 0;
  handleTables[0] = handleTable0;
  
  const _trampoline4 = function(arg0) {
    _debugLog('[iface="eo9:io/buffers@0.1.0", function="[constructor]buffer"] [Instruction::CallInterface] (sync, @ enter)');
    let hostProvided = true;
    
    let parentTask;
    let task;
    let subtask;
    
    const createTask = () => {
      const results = createNewCurrentTask({
        componentIdx: -1,
        isAsync: false,
        entryFnName: 'new Buffer',
        getCallbackFn: () => null,
        callbackFnName: null,
        errHandling: 'none',
        callingWasmExport: false,
      });
      task = results[0];
    };
    
    taskCreation: {
      parentTask = getCurrentTask(
      0,
      _getGlobalCurrentTaskMeta(0)?.taskID,
      )?.task;
      
      if (!parentTask) {
        createTask();
        break taskCreation;
      }
      
      createTask();
      
      if (hostProvided) {
        subtask = parentTask.getLatestSubtask();
        if (!subtask) {
          throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
        }
        task.setParentSubtask(subtask);
      }
    }
    
    const started = task.enterSync();
    
    let ret;
    
    try {
      ret = _withGlobalCurrentTaskMeta({
        componentIdx: task.componentIdx(),
        taskID: task.id(),
        fn: () => new Buffer(BigInt.asUintN(64, BigInt(arg0))),
      })
      ;
    } catch (err) {
      
      task.setErrored(err);
      task.reject(err);
      task.exit();
      throw err;
      
    }
    
    
    if (!(ret instanceof Buffer)) {
      throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
    }
    var handle0 = ret[symbolRscHandle];
    if (!handle0) {
      const rep = ret[symbolRscRep] || ++captureCnt0;
      captureTable0.set(rep, ret);
      handle0 = rscTableCreateOwn(handleTable0, rep);
    }
    
    _debugLog('[iface="eo9:io/buffers@0.1.0", function="[constructor]buffer"][Instruction::Return]', {
      funcName: '[constructor]buffer',
      paramCount: 1,
      async: false,
      postReturn: false
    });
    task.resolve([handle0]);
    task.exit();
    return handle0;
  }
  _trampoline4.fnName = 'eo9:io/buffers@0.1.0#new Buffer';
  const handleTable1 = [T_FLAG, 0];
  const captureTable1= new Map();
  let captureCnt1 = 0;
  handleTables[1] = handleTable1;
  
  const _trampoline5 = function() {
    _debugLog('[iface="eo9:fs/fs@0.1.0", function="default"] [Instruction::CallInterface] (sync, @ enter)');
    let hostProvided = true;
    
    let parentTask;
    let task;
    let subtask;
    
    const createTask = () => {
      const results = createNewCurrentTask({
        componentIdx: -1,
        isAsync: false,
        entryFnName: '_default',
        getCallbackFn: () => null,
        callbackFnName: null,
        errHandling: 'none',
        callingWasmExport: false,
      });
      task = results[0];
    };
    
    taskCreation: {
      parentTask = getCurrentTask(
      0,
      _getGlobalCurrentTaskMeta(0)?.taskID,
      )?.task;
      
      if (!parentTask) {
        createTask();
        break taskCreation;
      }
      
      createTask();
      
      if (hostProvided) {
        subtask = parentTask.getLatestSubtask();
        if (!subtask) {
          throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
        }
        task.setParentSubtask(subtask);
      }
    }
    
    const started = task.enterSync();
    
    let ret;
    
    try {
      ret = _withGlobalCurrentTaskMeta({
        componentIdx: task.componentIdx(),
        taskID: task.id(),
        fn: () => _default(),
      })
      ;
    } catch (err) {
      
      task.setErrored(err);
      task.reject(err);
      task.exit();
      throw err;
      
    }
    
    
    if (!(ret instanceof FsImpl)) {
      throw new TypeError('Resource error: Not a valid \"FsImpl\" resource.');
    }
    var handle0 = ret[symbolRscHandle];
    if (!handle0) {
      const rep = ret[symbolRscRep] || ++captureCnt1;
      captureTable1.set(rep, ret);
      handle0 = rscTableCreateOwn(handleTable1, rep);
    }
    
    _debugLog('[iface="eo9:fs/fs@0.1.0", function="default"][Instruction::Return]', {
      funcName: 'default',
      paramCount: 1,
      async: false,
      postReturn: false
    });
    task.resolve([handle0]);
    task.exit();
    return handle0;
  }
  _trampoline5.fnName = 'eo9:fs/fs@0.1.0#_default';
  let exports1;
  let memory0;
  let realloc0;
  let realloc0Async;
  const handleTable2 = [T_FLAG, 0];
  const captureTable2= new Map();
  let captureCnt2 = 0;
  handleTables[2] = handleTable2;
  
  const _trampoline14 = async function(arg0, arg1, arg2, arg3, arg4) {
    var handle1 = arg0;
    
    var rep2 = handleTable1[(handle1 << 1) + 1] & ~T_FLAG;
    var rsc0 = captureTable1.get(rep2);
    if (!rsc0) {
      rsc0 = Object.create(FsImpl.prototype);
      Object.defineProperty(rsc0, symbolRscHandle, { writable: true, value: handle1});
      Object.defineProperty(rsc0, symbolRscRep, { writable: true, value: rep2});
    }
    
    curResourceBorrows.push(rsc0);
    var ptr3 = arg1;
    var len3 = arg2;
    var result3 = TEXT_DECODER_UTF8.decode(new Uint8Array(memory0.buffer, ptr3, len3));
    if ((arg3 & 4294967280) !== 0) {
      throw new TypeError('flags have extraneous bits set');
    }
    var flags4 = {
      read: Boolean(arg3 & 1),
      write: Boolean(arg3 & 2),
      create: Boolean(arg3 & 4),
      truncate: Boolean(arg3 & 8),
    };
    _debugLog('[iface="eo9:fs/fs@0.1.0", function="open"] [Instruction::CallInterface] (async, @ enter)');
    let hostProvided = true;
    
    let parentTask;
    let task;
    let subtask;
    
    const createTask = () => {
      const results = createNewCurrentTask({
        componentIdx: -1,
        isAsync: true,
        entryFnName: 'open',
        getCallbackFn: () => null,
        callbackFnName: null,
        errHandling: 'result-catch-handler',
        callingWasmExport: false,
      });
      task = results[0];
    };
    
    taskCreation: {
      parentTask = getCurrentTask(
      0,
      _getGlobalCurrentTaskMeta(0)?.taskID,
      )?.task;
      
      if (!parentTask) {
        createTask();
        break taskCreation;
      }
      
      createTask();
      
      if (hostProvided) {
        subtask = parentTask.getLatestSubtask();
        if (!subtask) {
          throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
        }
        task.setParentSubtask(subtask);
      }
    }
    
    
    const started = await task.enter({ isHost: hostProvided });
    if (!started) {
      _debugLog('[Instruction::CallInterface] failed to enter task', {
        taskID: task.id(),
        subtaskID: currentSubtask?.id(),
      });
      throw new Error("failed to enter task");
    }
    
    
    let ret;
    try {
      ret = { tag: 'ok', val: await  _withGlobalCurrentTaskMetaAsync({
        componentIdx: task.componentIdx(),
        taskID: task.id(),
        fn: () => open(rsc0, result3, flags4),
      })
    };
  } catch (e) {
    ret = { tag: 'err', val: getErrorPayload(e) };
  }
  
  for (const rsc of curResourceBorrows) {
    rsc[symbolRscHandle] = undefined;
  }
  curResourceBorrows = [];
  var variant8 = ret;
  let variant8_0;
  let variant8_1;
  let variant8_2;
  let variant8_3;
  switch (variant8.tag) {
    case 'ok': {
      const e = variant8.val;
      
      if (!(e instanceof File)) {
        throw new TypeError('Resource error: Not a valid \"File\" resource.');
      }
      var handle5 = e[symbolRscHandle];
      if (!handle5) {
        const rep = e[symbolRscRep] || ++captureCnt2;
        captureTable2.set(rep, e);
        handle5 = rscTableCreateOwn(handleTable2, rep);
      }
      
      variant8_0 = 0;
      variant8_1 = handle5;
      variant8_2 = 0;
      variant8_3 = 0;
      
      break;
    }
    case 'err': {
      const e = variant8.val;
      var variant7 = e;
      let variant7_0;
      let variant7_1;
      let variant7_2;
      switch (variant7.tag) {
        case 'not-found': {
          variant7_0 = 0;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'already-exists': {
          variant7_0 = 1;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'not-a-directory': {
          variant7_0 = 2;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'is-a-directory': {
          variant7_0 = 3;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'denied': {
          variant7_0 = 4;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'read-only': {
          variant7_0 = 5;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'no-space': {
          variant7_0 = 6;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'not-immutable': {
          variant7_0 = 7;
          variant7_1 = 0;
          variant7_2 = 0;
          break;
        }
        case 'io': {
          const e = variant7.val;
          
          var encodeRes = await _utf8AllocateAndEncodeAsync(e, realloc0Async, memory0);
          var ptr6= encodeRes.ptr;
          var len6 = encodeRes.len;
          
          variant7_0 = 8;
          variant7_1 = ptr6;
          variant7_2 = len6;
          break;
        }
        default: {
          throw new TypeError(`invalid variant tag value \`${JSON.stringify(variant7.tag)}\` (received \`${variant7}\`) specified for \`FsError\``);
        }
      }
      variant8_0 = 1;
      variant8_1 = variant7_0;
      variant8_2 = variant7_1;
      variant8_3 = variant7_2;
      
      break;
    }
    default: {
      _debugLog("ERROR: invalid value (expected result as object with 'tag' member)", { value: variant8, valueType: typeof variant8});
      throw new TypeError('invalid variant specified for result');
    }
  }
  _debugLog('[iface="eo9:fs/fs@0.1.0", function="open"][Instruction::AsyncTaskReturn]', {
    funcName: '[task-return]open',
    paramCount: 4,
    componentIdx: 0,
    postReturn: false,
    hostProvided,
  });
  
  if (hostProvided) {
    _debugLog('[Instruction::AsyncTaskReturn] signaling host-provided async return completion', {
      task: task.id(),
      subtask: subtask?.id(),
      result: ret,
    })
    task.resolve([ret]);
    task.exit();
    return task.completionPromise();
  }
  
  const componentState = getOrCreateAsyncState(0);
  if (!componentState) { throw new Error('failed to lookup current component state'); }
  
  queueMicrotask(async (resolve, reject) => {
    try {
      _debugLog("[Instruction::AsyncTaskReturn] starting driver loop", {
        fnName: '[task-return]open',
        componentInstanceIdx: 0,
        taskID: task.id(),
      });
      await _driverLoop({
        componentInstanceIdx: 0,
        componentState,
        task,
        fnName: '[task-return]open',
        isAsync: true,
        callbackResult: ret,
      });
    } catch (err) {
      _debugLog("[Instruction::AsyncTaskReturn] driver loop call failure", { err });
    }
  });
  
  let taskRes = await task.completionPromise();
  if (task.getErrHandling() === 'throw-result-err') {
    if (typeof taskRes !== 'object') { return taskRes; }
    if (taskRes.tag === 'err') { throw taskRes.val; }
    if (taskRes.tag === 'ok') { taskRes = taskRes.val; }
  }
  
  return taskRes;
  
}
_trampoline14.fnName = 'eo9:fs/fs@0.1.0#open';

const _trampoline15 = async function(arg0, arg1, arg2, arg3) {
  var handle1 = arg0;
  
  var rep2 = handleTable2[(handle1 << 1) + 1] & ~T_FLAG;
  var rsc0 = captureTable2.get(rep2);
  if (!rsc0) {
    rsc0 = Object.create(File.prototype);
    Object.defineProperty(rsc0, symbolRscHandle, { writable: true, value: handle1});
    Object.defineProperty(rsc0, symbolRscRep, { writable: true, value: rep2});
  }
  
  curResourceBorrows.push(rsc0);
  var handle4 = arg2;
  
  var rep5 = handleTable0[(handle4 << 1) + 1] & ~T_FLAG;
  var rsc3 = captureTable0.get(rep5);
  if (!rsc3) {
    rsc3 = Object.create(Buffer.prototype);
    Object.defineProperty(rsc3, symbolRscHandle, { writable: true, value: handle4});
    Object.defineProperty(rsc3, symbolRscRep, { writable: true, value: rep5});
  }
  
  else {
    captureTable0.delete(rep5);
  }
  rscTableRemove(handleTable0, handle4);
  _debugLog('[iface="eo9:fs/fs@0.1.0", function="read"] [Instruction::CallInterface] (async, @ enter)');
  let hostProvided = true;
  
  let parentTask;
  let task;
  let subtask;
  
  const createTask = () => {
    const results = createNewCurrentTask({
      componentIdx: -1,
      isAsync: true,
      entryFnName: 'read',
      getCallbackFn: () => null,
      callbackFnName: null,
      errHandling: 'none',
      callingWasmExport: false,
    });
    task = results[0];
  };
  
  taskCreation: {
    parentTask = getCurrentTask(
    0,
    _getGlobalCurrentTaskMeta(0)?.taskID,
    )?.task;
    
    if (!parentTask) {
      createTask();
      break taskCreation;
    }
    
    createTask();
    
    if (hostProvided) {
      subtask = parentTask.getLatestSubtask();
      if (!subtask) {
        throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
      }
      task.setParentSubtask(subtask);
    }
  }
  
  
  const started = await task.enter({ isHost: hostProvided });
  if (!started) {
    _debugLog('[Instruction::CallInterface] failed to enter task', {
      taskID: task.id(),
      subtaskID: currentSubtask?.id(),
    });
    throw new Error("failed to enter task");
  }
  
  
  let ret;
  
  try {
    ret = await  _withGlobalCurrentTaskMetaAsync({
      componentIdx: task.componentIdx(),
      taskID: task.id(),
      fn: () => read(rsc0, BigInt.asUintN(64, BigInt(arg1)), rsc3),
    })
    ;
  } catch (err) {
    
    task.setErrored(err);
    task.reject(err);
    task.exit();
    return task.completionPromise();
    
  }
  
  for (const rsc of curResourceBorrows) {
    rsc[symbolRscHandle] = undefined;
  }
  curResourceBorrows = [];
  var [tuple6_0, tuple6_1] = ret;
  
  if (!(tuple6_0 instanceof Buffer)) {
    throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
  }
  var handle7 = tuple6_0[symbolRscHandle];
  if (!handle7) {
    const rep = tuple6_0[symbolRscRep] || ++captureCnt0;
    captureTable0.set(rep, tuple6_0);
    handle7 = rscTableCreateOwn(handleTable0, rep);
  }
  
  dataView(memory0).setInt32(arg3 + 0, handle7, true);
  var variant11 = tuple6_1;
  switch (variant11.tag) {
    case 'ok': {
      const e = variant11.val;
      dataView(memory0).setInt8(arg3 + 8, 0, true);
      var {bytesRead: v8_0 } = e;
      dataView(memory0).setBigInt64(arg3 + 16, toUint64(v8_0), true);
      
      break;
    }
    case 'err': {
      const e = variant11.val;
      dataView(memory0).setInt8(arg3 + 8, 1, true);
      var variant10 = e;
      switch (variant10.tag) {
        case 'not-found': {
          dataView(memory0).setInt8(arg3 + 16, 0, true);
          break;
        }
        case 'already-exists': {
          dataView(memory0).setInt8(arg3 + 16, 1, true);
          break;
        }
        case 'not-a-directory': {
          dataView(memory0).setInt8(arg3 + 16, 2, true);
          break;
        }
        case 'is-a-directory': {
          dataView(memory0).setInt8(arg3 + 16, 3, true);
          break;
        }
        case 'denied': {
          dataView(memory0).setInt8(arg3 + 16, 4, true);
          break;
        }
        case 'read-only': {
          dataView(memory0).setInt8(arg3 + 16, 5, true);
          break;
        }
        case 'no-space': {
          dataView(memory0).setInt8(arg3 + 16, 6, true);
          break;
        }
        case 'not-immutable': {
          dataView(memory0).setInt8(arg3 + 16, 7, true);
          break;
        }
        case 'io': {
          const e = variant10.val;
          dataView(memory0).setInt8(arg3 + 16, 8, true);
          
          var encodeRes = await _utf8AllocateAndEncodeAsync(e, realloc0Async, memory0);
          var ptr9= encodeRes.ptr;
          var len9 = encodeRes.len;
          
          dataView(memory0).setUint32(arg3 + 24, len9, true);
          dataView(memory0).setUint32(arg3 + 20, ptr9, true);
          break;
        }
        default: {
          throw new TypeError(`invalid variant tag value \`${JSON.stringify(variant10.tag)}\` (received \`${variant10}\`) specified for \`FsError\``);
        }
      }
      
      break;
    }
    default: {
      _debugLog("ERROR: invalid value (expected result as object with 'tag' member)", { value: variant11, valueType: typeof variant11});
      throw new TypeError('invalid variant specified for result');
    }
  }
  _debugLog('[iface="eo9:fs/fs@0.1.0", function="read"][Instruction::AsyncTaskReturn]', {
    funcName: '[task-return]read',
    paramCount: 0,
    componentIdx: 0,
    postReturn: false,
    hostProvided,
  });
  
  if (hostProvided) {
    _debugLog('[Instruction::AsyncTaskReturn] signaling host-provided async return completion', {
      task: task.id(),
      subtask: subtask?.id(),
      result: ret,
    })
    task.resolve([ret]);
    task.exit();
    return task.completionPromise();
  }
  
  const componentState = getOrCreateAsyncState(0);
  if (!componentState) { throw new Error('failed to lookup current component state'); }
  
  queueMicrotask(async (resolve, reject) => {
    try {
      _debugLog("[Instruction::AsyncTaskReturn] starting driver loop", {
        fnName: '[task-return]read',
        componentInstanceIdx: 0,
        taskID: task.id(),
      });
      await _driverLoop({
        componentInstanceIdx: 0,
        componentState,
        task,
        fnName: '[task-return]read',
        isAsync: true,
        callbackResult: ret,
      });
    } catch (err) {
      _debugLog("[Instruction::AsyncTaskReturn] driver loop call failure", { err });
    }
  });
  
  let taskRes = await task.completionPromise();
  if (task.getErrHandling() === 'throw-result-err') {
    if (typeof taskRes !== 'object') { return taskRes; }
    if (taskRes.tag === 'err') { throw taskRes.val; }
    if (taskRes.tag === 'ok') { taskRes = taskRes.val; }
  }
  
  return taskRes;
  
}
_trampoline15.fnName = 'eo9:fs/fs@0.1.0#read';

const _trampoline16 = async function(arg0, arg1, arg2, arg3) {
  var handle1 = arg0;
  
  var rep2 = handleTable2[(handle1 << 1) + 1] & ~T_FLAG;
  var rsc0 = captureTable2.get(rep2);
  if (!rsc0) {
    rsc0 = Object.create(File.prototype);
    Object.defineProperty(rsc0, symbolRscHandle, { writable: true, value: handle1});
    Object.defineProperty(rsc0, symbolRscRep, { writable: true, value: rep2});
  }
  
  curResourceBorrows.push(rsc0);
  var handle4 = arg2;
  
  var rep5 = handleTable0[(handle4 << 1) + 1] & ~T_FLAG;
  var rsc3 = captureTable0.get(rep5);
  if (!rsc3) {
    rsc3 = Object.create(Buffer.prototype);
    Object.defineProperty(rsc3, symbolRscHandle, { writable: true, value: handle4});
    Object.defineProperty(rsc3, symbolRscRep, { writable: true, value: rep5});
  }
  
  else {
    captureTable0.delete(rep5);
  }
  rscTableRemove(handleTable0, handle4);
  _debugLog('[iface="eo9:fs/fs@0.1.0", function="write"] [Instruction::CallInterface] (async, @ enter)');
  let hostProvided = true;
  
  let parentTask;
  let task;
  let subtask;
  
  const createTask = () => {
    const results = createNewCurrentTask({
      componentIdx: -1,
      isAsync: true,
      entryFnName: 'write',
      getCallbackFn: () => null,
      callbackFnName: null,
      errHandling: 'none',
      callingWasmExport: false,
    });
    task = results[0];
  };
  
  taskCreation: {
    parentTask = getCurrentTask(
    0,
    _getGlobalCurrentTaskMeta(0)?.taskID,
    )?.task;
    
    if (!parentTask) {
      createTask();
      break taskCreation;
    }
    
    createTask();
    
    if (hostProvided) {
      subtask = parentTask.getLatestSubtask();
      if (!subtask) {
        throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
      }
      task.setParentSubtask(subtask);
    }
  }
  
  
  const started = await task.enter({ isHost: hostProvided });
  if (!started) {
    _debugLog('[Instruction::CallInterface] failed to enter task', {
      taskID: task.id(),
      subtaskID: currentSubtask?.id(),
    });
    throw new Error("failed to enter task");
  }
  
  
  let ret;
  
  try {
    ret = await  _withGlobalCurrentTaskMetaAsync({
      componentIdx: task.componentIdx(),
      taskID: task.id(),
      fn: () => write(rsc0, BigInt.asUintN(64, BigInt(arg1)), rsc3),
    })
    ;
  } catch (err) {
    
    task.setErrored(err);
    task.reject(err);
    task.exit();
    return task.completionPromise();
    
  }
  
  for (const rsc of curResourceBorrows) {
    rsc[symbolRscHandle] = undefined;
  }
  curResourceBorrows = [];
  var [tuple6_0, tuple6_1] = ret;
  
  if (!(tuple6_0 instanceof Buffer)) {
    throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
  }
  var handle7 = tuple6_0[symbolRscHandle];
  if (!handle7) {
    const rep = tuple6_0[symbolRscRep] || ++captureCnt0;
    captureTable0.set(rep, tuple6_0);
    handle7 = rscTableCreateOwn(handleTable0, rep);
  }
  
  dataView(memory0).setInt32(arg3 + 0, handle7, true);
  var variant11 = tuple6_1;
  switch (variant11.tag) {
    case 'ok': {
      const e = variant11.val;
      dataView(memory0).setInt8(arg3 + 8, 0, true);
      var {bytesWritten: v8_0 } = e;
      dataView(memory0).setBigInt64(arg3 + 16, toUint64(v8_0), true);
      
      break;
    }
    case 'err': {
      const e = variant11.val;
      dataView(memory0).setInt8(arg3 + 8, 1, true);
      var variant10 = e;
      switch (variant10.tag) {
        case 'not-found': {
          dataView(memory0).setInt8(arg3 + 16, 0, true);
          break;
        }
        case 'already-exists': {
          dataView(memory0).setInt8(arg3 + 16, 1, true);
          break;
        }
        case 'not-a-directory': {
          dataView(memory0).setInt8(arg3 + 16, 2, true);
          break;
        }
        case 'is-a-directory': {
          dataView(memory0).setInt8(arg3 + 16, 3, true);
          break;
        }
        case 'denied': {
          dataView(memory0).setInt8(arg3 + 16, 4, true);
          break;
        }
        case 'read-only': {
          dataView(memory0).setInt8(arg3 + 16, 5, true);
          break;
        }
        case 'no-space': {
          dataView(memory0).setInt8(arg3 + 16, 6, true);
          break;
        }
        case 'not-immutable': {
          dataView(memory0).setInt8(arg3 + 16, 7, true);
          break;
        }
        case 'io': {
          const e = variant10.val;
          dataView(memory0).setInt8(arg3 + 16, 8, true);
          
          var encodeRes = await _utf8AllocateAndEncodeAsync(e, realloc0Async, memory0);
          var ptr9= encodeRes.ptr;
          var len9 = encodeRes.len;
          
          dataView(memory0).setUint32(arg3 + 24, len9, true);
          dataView(memory0).setUint32(arg3 + 20, ptr9, true);
          break;
        }
        default: {
          throw new TypeError(`invalid variant tag value \`${JSON.stringify(variant10.tag)}\` (received \`${variant10}\`) specified for \`FsError\``);
        }
      }
      
      break;
    }
    default: {
      _debugLog("ERROR: invalid value (expected result as object with 'tag' member)", { value: variant11, valueType: typeof variant11});
      throw new TypeError('invalid variant specified for result');
    }
  }
  _debugLog('[iface="eo9:fs/fs@0.1.0", function="write"][Instruction::AsyncTaskReturn]', {
    funcName: '[task-return]write',
    paramCount: 0,
    componentIdx: 0,
    postReturn: false,
    hostProvided,
  });
  
  if (hostProvided) {
    _debugLog('[Instruction::AsyncTaskReturn] signaling host-provided async return completion', {
      task: task.id(),
      subtask: subtask?.id(),
      result: ret,
    })
    task.resolve([ret]);
    task.exit();
    return task.completionPromise();
  }
  
  const componentState = getOrCreateAsyncState(0);
  if (!componentState) { throw new Error('failed to lookup current component state'); }
  
  queueMicrotask(async (resolve, reject) => {
    try {
      _debugLog("[Instruction::AsyncTaskReturn] starting driver loop", {
        fnName: '[task-return]write',
        componentInstanceIdx: 0,
        taskID: task.id(),
      });
      await _driverLoop({
        componentInstanceIdx: 0,
        componentState,
        task,
        fnName: '[task-return]write',
        isAsync: true,
        callbackResult: ret,
      });
    } catch (err) {
      _debugLog("[Instruction::AsyncTaskReturn] driver loop call failure", { err });
    }
  });
  
  let taskRes = await task.completionPromise();
  if (task.getErrHandling() === 'throw-result-err') {
    if (typeof taskRes !== 'object') { return taskRes; }
    if (taskRes.tag === 'err') { throw taskRes.val; }
    if (taskRes.tag === 'ok') { taskRes = taskRes.val; }
  }
  
  return taskRes;
  
}
_trampoline16.fnName = 'eo9:fs/fs@0.1.0#write';

const _trampoline17 = function(arg0, arg1, arg2, arg3) {
  var handle1 = arg0;
  
  var rep2 = handleTable0[(handle1 << 1) + 1] & ~T_FLAG;
  var rsc0 = captureTable0.get(rep2);
  if (!rsc0) {
    rsc0 = Object.create(Buffer.prototype);
    Object.defineProperty(rsc0, symbolRscHandle, { writable: true, value: handle1});
    Object.defineProperty(rsc0, symbolRscRep, { writable: true, value: rep2});
  }
  
  curResourceBorrows.push(rsc0);
  _debugLog('[iface="eo9:io/buffers@0.1.0", function="[method]buffer.read"] [Instruction::CallInterface] (sync, @ enter)');
  let hostProvided = true;
  
  let parentTask;
  let task;
  let subtask;
  
  const createTask = () => {
    const results = createNewCurrentTask({
      componentIdx: -1,
      isAsync: false,
      entryFnName: 'read',
      getCallbackFn: () => null,
      callbackFnName: null,
      errHandling: 'none',
      callingWasmExport: false,
    });
    task = results[0];
  };
  
  taskCreation: {
    parentTask = getCurrentTask(
    0,
    _getGlobalCurrentTaskMeta(0)?.taskID,
    )?.task;
    
    if (!parentTask) {
      createTask();
      break taskCreation;
    }
    
    createTask();
    
    if (hostProvided) {
      subtask = parentTask.getLatestSubtask();
      if (!subtask) {
        throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
      }
      task.setParentSubtask(subtask);
    }
  }
  
  const started = task.enterSync();
  
  let ret;
  
  try {
    ret = _withGlobalCurrentTaskMeta({
      componentIdx: task.componentIdx(),
      taskID: task.id(),
      fn: () => rsc0.read(BigInt.asUintN(64, BigInt(arg1)), BigInt.asUintN(64, BigInt(arg2))),
    })
    ;
  } catch (err) {
    
    task.setErrored(err);
    task.reject(err);
    task.exit();
    throw err;
    
  }
  
  for (const rsc of curResourceBorrows) {
    rsc[symbolRscHandle] = undefined;
  }
  curResourceBorrows = [];
  var val3 = ret;
  var len3 = Array.isArray(val3) ? val3.length : val3.byteLength;
  var ptr3 = realloc0(0, 0, 1, len3 * 1);
  
  let valData3;
  const valLenBytes3 = len3 * 1;
  if (Array.isArray(val3)) {
    // Regular array likely containing numbers, write values to memory
    let offset = 0;
    const dv3 = new DataView(memory0.buffer);
    for (const v of val3) {
      _requireValidNumericPrimitive.bind(null, 'u8')(v);
      dv3.setUint8(ptr3+ offset, v, true);
      offset += 1;
    }
  } else {
    // TypedArray / ArrayBuffer-like, direct copy
    valData3 = new Uint8Array(val3.buffer || val3, val3.byteOffset, valLenBytes3);
    const out3 = new Uint8Array(memory0.buffer, ptr3, valLenBytes3);
    out3.set(valData3);
  }
  
  dataView(memory0).setUint32(arg3 + 4, len3, true);
  dataView(memory0).setUint32(arg3 + 0, ptr3, true);
  _debugLog('[iface="eo9:io/buffers@0.1.0", function="[method]buffer.read"][Instruction::Return]', {
    funcName: '[method]buffer.read',
    paramCount: 0,
    async: false,
    postReturn: false
  });
  task.resolve([ret]);
  task.exit();
}
_trampoline17.fnName = 'eo9:io/buffers@0.1.0#read';

const _trampoline18 = function(arg0, arg1, arg2, arg3) {
  var handle1 = arg0;
  
  var rep2 = handleTable0[(handle1 << 1) + 1] & ~T_FLAG;
  var rsc0 = captureTable0.get(rep2);
  if (!rsc0) {
    rsc0 = Object.create(Buffer.prototype);
    Object.defineProperty(rsc0, symbolRscHandle, { writable: true, value: handle1});
    Object.defineProperty(rsc0, symbolRscRep, { writable: true, value: rep2});
  }
  
  curResourceBorrows.push(rsc0);
  var ptr3 = arg2;
  var len3 = arg3;
  var result3 = new Uint8Array(memory0.buffer.slice(ptr3, ptr3 + len3 * 1));
  _debugLog('[iface="eo9:io/buffers@0.1.0", function="[method]buffer.write"] [Instruction::CallInterface] (sync, @ enter)');
  let hostProvided = true;
  
  let parentTask;
  let task;
  let subtask;
  
  const createTask = () => {
    const results = createNewCurrentTask({
      componentIdx: -1,
      isAsync: false,
      entryFnName: 'write',
      getCallbackFn: () => null,
      callbackFnName: null,
      errHandling: 'none',
      callingWasmExport: false,
    });
    task = results[0];
  };
  
  taskCreation: {
    parentTask = getCurrentTask(
    0,
    _getGlobalCurrentTaskMeta(0)?.taskID,
    )?.task;
    
    if (!parentTask) {
      createTask();
      break taskCreation;
    }
    
    createTask();
    
    if (hostProvided) {
      subtask = parentTask.getLatestSubtask();
      if (!subtask) {
        throw new Error(`Missing subtask (in parent task [${parentTask.id()}]) for host import, has the import been lowered? (ensure asyncImports are set properly)`);
      }
      task.setParentSubtask(subtask);
    }
  }
  
  const started = task.enterSync();
  
  let ret;
  
  try {
    _withGlobalCurrentTaskMeta({
      componentIdx: task.componentIdx(),
      taskID: task.id(),
      fn: () => rsc0.write(BigInt.asUintN(64, BigInt(arg1)), result3),
    })
    ;
  } catch (err) {
    
    task.setErrored(err);
    task.reject(err);
    task.exit();
    throw err;
    
  }
  
  for (const rsc of curResourceBorrows) {
    rsc[symbolRscHandle] = undefined;
  }
  curResourceBorrows = [];
  _debugLog('[iface="eo9:io/buffers@0.1.0", function="[method]buffer.write"][Instruction::Return]', {
    funcName: '[method]buffer.write',
    paramCount: 0,
    async: false,
    postReturn: false
  });
  task.resolve([ret]);
  task.exit();
}
_trampoline18.fnName = 'eo9:io/buffers@0.1.0#write';
let exports2;
let callback_0;
let exports1AsyncLiftMain;

async function main(arg0, arg1) {
  
  var encodeRes = await _utf8AllocateAndEncodeAsync(arg0, realloc0Async, memory0);
  var ptr0= encodeRes.ptr;
  var len0 = encodeRes.len;
  
  
  var encodeRes = await _utf8AllocateAndEncodeAsync(arg1, realloc0Async, memory0);
  var ptr1= encodeRes.ptr;
  var len1 = encodeRes.len;
  
  _debugLog('[iface="main", function="main"][Instruction::CallWasm] enter', {
    funcName: 'main',
    paramCount: 4,
    async: true,
    postReturn: false,
  });
  const hostProvided = false;
  
  const [task, _wasm_call_currentTaskID] = createNewCurrentTask({
    componentIdx: 0,
    isAsync: true,
    isManualAsync: false,
    entryFnName: 'exports1AsyncLiftMain',
    getCallbackFn: () => callback_0,
    callbackFnName: callback_0,
    errHandling: 'throw-result-err',
    callingWasmExport: true,
  });
  
  
  const started = await task.enter();
  if (!started) {
    _debugLog('[Instruction::AsyncTaskReturn] failed to enter task', {
      taskID: task.id(),
      subtaskID: currentSubtask?.id(),
    });
    throw new Error("failed to enter task");
  }
  
  
  if (0!== null) {
    task.setReturnMemoryIdx(0);
    task.setReturnMemory(() => memory0());
  }
  
  
  let ret;
  
  try {
    ret =  await  _withGlobalCurrentTaskMetaAsync({
      taskID: task.id(),
      componentIdx: task.componentIdx(),
      fn: () => exports1AsyncLiftMain(ptr0, len0, ptr1, len1),
    });
  } catch (err) {
    
    task.setErrored(err);
    task.reject(err);
    task.exit();
    return task.completionPromise();
    
  }
  
  _debugLog('[iface="main", function="main"][Instruction::AsyncTaskReturn]', {
    funcName: 'main',
    paramCount: 1,
    componentIdx: 0,
    postReturn: false,
    hostProvided,
  });
  
  if (hostProvided) {
    _debugLog('[Instruction::AsyncTaskReturn] signaling host-provided async return completion', {
      task: task.id(),
      subtask: subtask?.id(),
      result: ret,
    })
    task.resolve([ret]);
    task.exit();
    return task.completionPromise();
  }
  
  const componentState = getOrCreateAsyncState(0);
  if (!componentState) { throw new Error('failed to lookup current component state'); }
  
  queueMicrotask(async (resolve, reject) => {
    try {
      _debugLog("[Instruction::AsyncTaskReturn] starting driver loop", {
        fnName: 'main',
        componentInstanceIdx: 0,
        taskID: task.id(),
      });
      await _driverLoop({
        componentInstanceIdx: 0,
        componentState,
        task,
        fnName: 'main',
        isAsync: true,
        callbackResult: ret,
      });
    } catch (err) {
      _debugLog("[Instruction::AsyncTaskReturn] driver loop call failure", { err });
    }
  });
  
  let taskRes = await task.completionPromise();
  if (task.getErrHandling() === 'throw-result-err') {
    if (typeof taskRes !== 'object') { return taskRes; }
    if (taskRes.tag === 'err') { throw taskRes.val; }
    if (taskRes.tag === 'ok') { taskRes = taskRes.val; }
  }
  
  return taskRes;
  
}
const trampoline0 = subtaskCancel.bind(null, 0, false);

function trampoline1(handle) {
  const handleEntry = rscTableRemove(handleTable2, handle);
  if (handleEntry.own) {
    
    const rsc = captureTable2.get(handleEntry.rep);
    if (rsc) {
      if (rsc[symbolDispose]) rsc[symbolDispose]();
      captureTable2.delete(handleEntry.rep);
    } else if (File[symbolCabiDispose]) {
      File[symbolCabiDispose](handleEntry.rep);
    }
  }
}
function trampoline2(handle) {
  const handleEntry = rscTableRemove(handleTable0, handle);
  if (handleEntry.own) {
    
    const rsc = captureTable0.get(handleEntry.rep);
    if (rsc) {
      if (rsc[symbolDispose]) rsc[symbolDispose]();
      captureTable0.delete(handleEntry.rep);
    } else if (Buffer[symbolCabiDispose]) {
      Buffer[symbolCabiDispose](handleEntry.rep);
    }
  }
}
function trampoline3(handle) {
  const handleEntry = rscTableRemove(handleTable1, handle);
  if (handleEntry.own) {
    
    const rsc = captureTable1.get(handleEntry.rep);
    if (rsc) {
      if (rsc[symbolDispose]) rsc[symbolDispose]();
      captureTable1.delete(handleEntry.rep);
    } else if (FsImpl[symbolCabiDispose]) {
      FsImpl[symbolCabiDispose](handleEntry.rep);
    }
  }
}
let trampoline4 = _trampoline4.manuallyAsync ? new WebAssembly.Suspending(_lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 4,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline4.manuallyAsync,
  paramLiftFns: [_liftFlatU64],
  resultLowerFns: [_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_Buffer(obj) {
      if (!(obj instanceof Buffer)) {
        throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt0;
        captureTable0.set(rep, obj);
        handle = rscTableCreateOwn(handleTable0, rep);
      }
      return handle;
    }
    ,
  })],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: null,
  stringEncoding: 'utf8',
  getMemoryFn: () => null,
  getReallocFn: undefined,
  importFn: _trampoline4,
},
)) : _lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 4,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline4.manuallyAsync,
  paramLiftFns: [_liftFlatU64],
  resultLowerFns: [_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_Buffer(obj) {
      if (!(obj instanceof Buffer)) {
        throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt0;
        captureTable0.set(rep, obj);
        handle = rscTableCreateOwn(handleTable0, rep);
      }
      return handle;
    }
    ,
  })],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: null,
  stringEncoding: 'utf8',
  getMemoryFn: () => null,
  getReallocFn: undefined,
  importFn: _trampoline4,
},
);
let trampoline5 = _trampoline5.manuallyAsync ? new WebAssembly.Suspending(_lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 5,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline5.manuallyAsync,
  paramLiftFns: [],
  resultLowerFns: [_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_FsImpl(obj) {
      if (!(obj instanceof FsImpl)) {
        throw new TypeError('Resource error: Not a valid \"FsImpl\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt1;
        captureTable1.set(rep, obj);
        handle = rscTableCreateOwn(handleTable1, rep);
      }
      return handle;
    }
    ,
  })],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: null,
  stringEncoding: 'utf8',
  getMemoryFn: () => null,
  getReallocFn: undefined,
  importFn: _trampoline5,
},
)) : _lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 5,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline5.manuallyAsync,
  paramLiftFns: [],
  resultLowerFns: [_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_FsImpl(obj) {
      if (!(obj instanceof FsImpl)) {
        throw new TypeError('Resource error: Not a valid \"FsImpl\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt1;
        captureTable1.set(rep, obj);
        handle = rscTableCreateOwn(handleTable1, rep);
      }
      return handle;
    }
    ,
  })],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: null,
  stringEncoding: 'utf8',
  getMemoryFn: () => null,
  getReallocFn: undefined,
  importFn: _trampoline5,
},
);
const trampoline6 = waitableJoin.bind(null, 0);


const trampoline7 = contextGet.bind(null, {
  componentIdx: 0,
  slot: 0,
});


const trampoline8 = contextSet.bind(null, {
  componentIdx: 0,
  slot: 0,
});

const trampoline9 = waitableSetNew.bind(null, 0);

const trampoline10 = waitableSetDrop.bind(null, 0);

const trampoline11 = taskCancel.bind(null, 0);

const trampoline12 = subtaskDrop.bind(
null,
0,
);

const trampoline13 = waitableSetPoll.bind(
null,
{
  componentIdx: 0,
  isAsync: false,
  isCancellable: false,
  memoryIdx: 0,
  getMemoryFn: () => memory0,
}
);

let trampoline14 = new WebAssembly.Suspending(_lowerImport.bind(
null,
{
  trampolineIdx: 14,
  componentIdx: 0,
  isAsync: true,
  isManualAsync: _trampoline14.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 1),_liftFlatStringAny,_liftFlatFlags({ names: ['read','write','create','truncate'], size32: 1, align32: 1, intSizeBytes: 1 })],
  resultLowerFns: [_lowerFlatResult([
  [ 'ok', _lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_File(obj) {
      if (!(obj instanceof File)) {
        throw new TypeError('Resource error: Not a valid \"File\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt2;
        captureTable2.set(rep, obj);
        handle = rscTableCreateOwn(handleTable2, rep);
      }
      return handle;
    }
    ,
  }), 16, 4, 4 ],
  [ 'err', _lowerFlatVariant([[ 'not-found', null, 12, 4, 4 ],[ 'already-exists', null, 12, 4, 4 ],[ 'not-a-directory', null, 12, 4, 4 ],[ 'is-a-directory', null, 12, 4, 4 ],[ 'denied', null, 12, 4, 4 ],[ 'read-only', null, 12, 4, 4 ],[ 'no-space', null, 12, 4, 4 ],[ 'not-immutable', null, 12, 4, 4 ],[ 'io', _lowerFlatStringAny, 12, 4, 4 ],]), 16, 4, 4 ],
  ])
  ],
  hasResultPointer: true,
  funcTypeIsAsync: true,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: () => realloc0,
  importFn: _trampoline14,
},
));
let trampoline15 = new WebAssembly.Suspending(_lowerImport.bind(
null,
{
  trampolineIdx: 15,
  componentIdx: 0,
  isAsync: true,
  isManualAsync: _trampoline15.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 2),_liftFlatU64,_liftFlatOwn({
    componentIdx: 0,
    className: Buffer,
    createResourceFn: 
    (handle) => {
      const rep = handleTable0[(handle << 1) + 1] & ~T_FLAG;
      let resourceObj = captureTable0.get(rep);
      if (!resourceObj) {
        resourceObj = Object.create(Buffer.prototype);
        Object.defineProperty(resourceObj, symbolRscHandle, { writable: true, value: handle });
        Object.defineProperty(resourceObj, symbolRscRep, { writable: true, value: rep });
      } else {
        captureTable0.delete(rep);
      }
      rscTableRemove(handleTable0, handle);
      return resourceObj;
    }
    ,
  })
  ],
  resultLowerFns: [_lowerFlatTuple({ elemLowerMetas: [[_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_Buffer(obj) {
      if (!(obj instanceof Buffer)) {
        throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt0;
        captureTable0.set(rep, obj);
        handle = rscTableCreateOwn(handleTable0, rep);
      }
      return handle;
    }
    ,
  }), 4, 4],[_lowerFlatResult([
  [ 'ok', _lowerFlatRecord({ fieldMetas: [['bytesRead', _lowerFlatU64, 8, 8 ],], size32: 8, align32: 8 }), 24, 8, 8 ],
  [ 'err', _lowerFlatVariant([[ 'not-found', null, 12, 4, 4 ],[ 'already-exists', null, 12, 4, 4 ],[ 'not-a-directory', null, 12, 4, 4 ],[ 'is-a-directory', null, 12, 4, 4 ],[ 'denied', null, 12, 4, 4 ],[ 'read-only', null, 12, 4, 4 ],[ 'no-space', null, 12, 4, 4 ],[ 'not-immutable', null, 12, 4, 4 ],[ 'io', _lowerFlatStringAny, 12, 4, 4 ],]), 24, 8, 8 ],
  ])
  , 24, 8],], size32: 32, align32: 8 })],
  hasResultPointer: true,
  funcTypeIsAsync: true,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: () => realloc0,
  importFn: _trampoline15,
},
));
let trampoline16 = new WebAssembly.Suspending(_lowerImport.bind(
null,
{
  trampolineIdx: 16,
  componentIdx: 0,
  isAsync: true,
  isManualAsync: _trampoline16.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 2),_liftFlatU64,_liftFlatOwn({
    componentIdx: 0,
    className: Buffer,
    createResourceFn: 
    (handle) => {
      const rep = handleTable0[(handle << 1) + 1] & ~T_FLAG;
      let resourceObj = captureTable0.get(rep);
      if (!resourceObj) {
        resourceObj = Object.create(Buffer.prototype);
        Object.defineProperty(resourceObj, symbolRscHandle, { writable: true, value: handle });
        Object.defineProperty(resourceObj, symbolRscRep, { writable: true, value: rep });
      } else {
        captureTable0.delete(rep);
      }
      rscTableRemove(handleTable0, handle);
      return resourceObj;
    }
    ,
  })
  ],
  resultLowerFns: [_lowerFlatTuple({ elemLowerMetas: [[_lowerFlatOwn({
    componentIdx: 0,
    lowerFn: 
    function lowerImportedOwnedHost_Buffer(obj) {
      if (!(obj instanceof Buffer)) {
        throw new TypeError('Resource error: Not a valid \"Buffer\" resource.');
      }
      let handle = obj[symbolRscHandle];
      if (!handle) {
        const rep = obj[symbolRscRep] || ++captureCnt0;
        captureTable0.set(rep, obj);
        handle = rscTableCreateOwn(handleTable0, rep);
      }
      return handle;
    }
    ,
  }), 4, 4],[_lowerFlatResult([
  [ 'ok', _lowerFlatRecord({ fieldMetas: [['bytesWritten', _lowerFlatU64, 8, 8 ],], size32: 8, align32: 8 }), 24, 8, 8 ],
  [ 'err', _lowerFlatVariant([[ 'not-found', null, 12, 4, 4 ],[ 'already-exists', null, 12, 4, 4 ],[ 'not-a-directory', null, 12, 4, 4 ],[ 'is-a-directory', null, 12, 4, 4 ],[ 'denied', null, 12, 4, 4 ],[ 'read-only', null, 12, 4, 4 ],[ 'no-space', null, 12, 4, 4 ],[ 'not-immutable', null, 12, 4, 4 ],[ 'io', _lowerFlatStringAny, 12, 4, 4 ],]), 24, 8, 8 ],
  ])
  , 24, 8],], size32: 32, align32: 8 })],
  hasResultPointer: true,
  funcTypeIsAsync: true,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: () => realloc0,
  importFn: _trampoline16,
},
));
let trampoline17 = _trampoline17.manuallyAsync ? new WebAssembly.Suspending(_lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 17,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline17.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 0),_liftFlatU64,_liftFlatU64],
  resultLowerFns: [_lowerFlatList({
    elemLowerFn: _lowerFlatU8,
    elemSize32: 1,
    elemAlign32: 1,
  })],
  hasResultPointer: true,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: () => realloc0,
  importFn: _trampoline17,
},
)) : _lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 17,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline17.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 0),_liftFlatU64,_liftFlatU64],
  resultLowerFns: [_lowerFlatList({
    elemLowerFn: _lowerFlatU8,
    elemSize32: 1,
    elemAlign32: 1,
  })],
  hasResultPointer: true,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: () => realloc0,
  importFn: _trampoline17,
},
);
let trampoline18 = _trampoline18.manuallyAsync ? new WebAssembly.Suspending(_lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 18,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline18.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 0),_liftFlatU64,_liftFlatList({
    elemLiftFn: _liftFlatU8,
    elemAlign32: 1,
    elemSize32: 1,
    typedArray: Uint8Array,
  })],
  resultLowerFns: [],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: undefined,
  importFn: _trampoline18,
},
)) : _lowerImportBackwardsCompat.bind(
null,
{
  trampolineIdx: 18,
  componentIdx: 0,
  isAsync: false,
  isManualAsync: _trampoline18.manuallyAsync,
  paramLiftFns: [_liftFlatBorrow.bind(null, 0),_liftFlatU64,_liftFlatList({
    elemLiftFn: _liftFlatU8,
    elemAlign32: 1,
    elemSize32: 1,
    typedArray: Uint8Array,
  })],
  resultLowerFns: [],
  hasResultPointer: false,
  funcTypeIsAsync: false,
  getCallbackFn: () => null,
  getPostReturnFn: () => null,
  isCancellable: false,
  memoryIdx: 0,
  stringEncoding: 'utf8',
  getMemoryFn: () => memory0,
  getReallocFn: undefined,
  importFn: _trampoline18,
},
);
const trampoline19 = taskReturn.bind(
null,
{
  componentIdx: 0,
  useDirectParams: true,
  getMemoryFn: () => memory0,
  memoryIdx: 0,
  callbackFnIdx: null,
  liftFns: [_liftFlatResult([['ok', _liftFlatVariant([['round-tripped', _liftFlatU64, 16, 8, 8, 1, 2],]), 24, 8, 8, 2, 4],['err', _liftFlatVariant([['bad-arguments', _liftFlatStringAny, 12, 4, 4, 2, 3],['fs', _liftFlatStringAny, 12, 4, 4, 2, 3],['mismatch', null, 12, 4, 4, 0, 3],]), 24, 8, 8, 3, 4],])],
  lowerFns: [_lowerFlatResult([
  [ 'ok', _lowerFlatVariant([[ 'round-tripped', _lowerFlatU64, 16, 8, 8 ],]), 24, 8, 8 ],
  [ 'err', _lowerFlatVariant([[ 'bad-arguments', _lowerFlatStringAny, 12, 4, 4 ],[ 'fs', _lowerFlatStringAny, 12, 4, 4 ],[ 'mismatch', null, 12, 4, 4 ],]), 24, 8, 8 ],
  ])
  ],
  stringEncoding: 'utf8',
},
);
Promise.all([module0, module1, module2]).catch(() => {});
({ exports: exports0 } = yield instantiateCore(yield module1));
({ exports: exports1 } = yield instantiateCore(yield module0, {
  $root: {
    '[context-get-0]': trampoline7,
    '[context-set-0]': trampoline8,
    '[subtask-cancel]': trampoline0,
    '[subtask-drop]': trampoline12,
    '[waitable-join]': trampoline6,
    '[waitable-set-drop]': trampoline10,
    '[waitable-set-new]': trampoline9,
    '[waitable-set-poll]': exports0['0'],
  },
  '[export]$root': {
    '[task-cancel]': trampoline11,
    '[task-return]main': exports0['6'],
  },
  'eo9:fs/fs@0.1.0': {
    '[async-lower]open': exports0['1'],
    '[async-lower]read': exports0['2'],
    '[async-lower]write': exports0['3'],
    '[resource-drop]file': trampoline1,
    'default': trampoline5,
  },
  'eo9:fs/types@0.1.0': {
    '[resource-drop]fs-impl': trampoline3,
  },
  'eo9:io/buffers@0.1.0': {
    '[constructor]buffer': trampoline4,
    '[method]buffer.read': exports0['4'],
    '[method]buffer.write': exports0['5'],
    '[resource-drop]buffer': trampoline2,
  },
}));
memory0 = exports1.memory;
realloc0 = exports1.cabi_realloc;

try {
  realloc0Async = WebAssembly.promising(exports1.cabi_realloc);
} catch(err) {
  realloc0Async = exports1.cabi_realloc;
}

({ exports: exports2 } = yield instantiateCore(yield module2, {
  '': {
    $imports: exports0.$imports,
    '0': trampoline13,
    '1': trampoline14,
    '2': trampoline15,
    '3': trampoline16,
    '4': trampoline17,
    '5': trampoline18,
    '6': trampoline19,
  },
}));

callback_0 = WebAssembly.promising(exports1['[callback][async-lift]main']);
callback_0.fnName = "exports1['[callback][async-lift]main']";

exports1AsyncLiftMain = WebAssembly.promising(exports1['[async-lift]main']);

return { main,  };
})();
let promise, resolve, reject;
function runNext (value) {
  try {
    let done;
    do {
      ({ value, done } = gen.next(value));
    } while (!(value instanceof Promise) && !done);
    if (done) {
      if (resolve) return resolve(value);
      else return value;
    }
    if (!promise) promise = new Promise((_resolve, _reject) => (resolve = _resolve, reject = _reject));
    value.then(nextVal => done ? resolve() : runNext(nextVal), reject);
  }
  catch (e) {
    if (reject) reject(e);
    else throw e;
  }
}
const maybeSyncReturn = runNext(null);
return promise || maybeSyncReturn;
};
