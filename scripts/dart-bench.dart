// dart-bench.dart — Dart 1.24.3 side of the three-way head-to-head
// (docs/cog_bench.md). A FAITHFUL port of the seven cog-bench.mst workloads
// (world/41a + world/42 BenchmarkDashboard bodies) to Dart 1.x, asserting
// the SAME checksums, with the SAME timing protocol: 10 inner reps per
// batch, cold = first batch, warm = median of 6 more, microsecond clock
// (Stopwatch.elapsedMicroseconds), output line shape `name cold_us=N
// warm_us=M` — so the reduce script can treat all three VMs identically.
//
// Dart 1 notes (this must run on the archived 2017 VM, linux-arm64):
//  - pre-2.0: no null safety, optional types unchecked, `new` everywhere;
//  - int is arbitrary-precision on the VM (smi/mint), so the arith and
//    dict checksums (1.12e18 / 1.7e11) are exact, as in both Smalltalks;
//  - Smalltalk `//` is floor division, Dart `~/` truncates — every `~/`
//    here operates on non-negative values in these workloads, where the
//    two agree (the in-workload asserts would catch any divergence);
//  - 1-based Smalltalk arrays are ported as length n+1 Dart Lists with
//    slot 0 unused, preserving iteration bounds and operation counts.
//
// Run (Lima VM, native arm64):
//   limactl shell ubuntu -- .dart/dart-sdk/bin/dart scripts/dart-bench.dart

import 'dart:io';

// ═══ the five micros (world/42_benchdash.mst bodies) ═══════════════════

int benchArith() {
  var s = 0;
  for (var i = 1; i <= 1500000; i++) {
    s = s + (i * i) - (i * 3);
  }
  return s;
}

int fib(int n) {
  if (n < 2) return n;
  return fib(n - 1) + fib(n - 2);
}

int benchFib() => fib(32);

int sieveOnce() {
  var size = 8190;
  var flags = new List(size + 1); // 1-based; slot 0 unused
  for (var x = 1; x <= size; x++) flags[x] = true;
  var count = 0;
  for (var i = 1; i <= size; i++) {
    if (flags[i]) {
      var prime = i + i + 1;
      var k = i + prime;
      while (k <= size) {
        flags[k] = false;
        k = k + prime;
      }
      count = count + 1;
    }
  }
  return count;
}

int benchSieve() {
  var count = 0;
  for (var r = 0; r < 4; r++) count = sieveOnce();
  return count;
}

int benchDict() {
  var d = new Map();
  for (var i = 1; i <= 8000; i++) d[i] = i * i;
  var sum = 0;
  for (var i = 1; i <= 8000; i++) sum = sum + d[i];
  return sum;
}

class Association {
  var key;
  var value;
  Association(this.key, this.value);
}

int benchAlloc() {
  var last = null;
  for (var i = 1; i <= 200000; i++) last = new Association(i, last);
  return last.key;
}

// ═══ Richards (world/41a lines 11-466, class-for-class) ═══════════════

// RBObject's shared identity constants (task identities are 1-based
// taskTable indices, exactly as in the .mst).
const int IDLER_ID = 1;
const int WORKER_ID = 2;
const int HANDLER_A = 3;
const int HANDLER_B = 4;
const int DEVICE_A = 5;
const int DEVICE_B = 6;
const int DEVICE_PACKET_KIND = 1;
const int WORK_PACKET_KIND = 2;

class Packet {
  Packet link;
  int identity;
  int kind;
  int datum;
  List data;
  Packet(this.link, this.identity, this.kind) {
    datum = 1;
    data = new List(5); // 1-based, slots 1..4
    for (var i = 1; i <= 4; i++) data[i] = 0;
  }
}

// RBObject>>append:head:
Packet append(Packet packet, Packet queueHead) {
  packet.link = null;
  if (queueHead == null) return packet;
  var mouse = queueHead;
  var link = mouse.link;
  while (link != null) {
    mouse = link;
    link = mouse.link;
  }
  mouse.link = packet;
  return queueHead;
}

// TaskState (the three flags + transitions/predicates), inherited by
// TaskControlBlock exactly as in the .mst.
class TaskState {
  bool packetPendingIV = false;
  bool taskWaiting = false;
  bool taskHolding = false;

  void becomePacketPending() {
    packetPendingIV = true;
    taskWaiting = false;
    taskHolding = false;
  }

  void becomeRunning() {
    packetPendingIV = false;
    taskWaiting = false;
    taskHolding = false;
  }

  void becomeWaiting() {
    packetPendingIV = false;
    taskHolding = false;
    taskWaiting = true;
  }

  void becomeWaitingWithPacket() {
    taskHolding = false;
    taskWaiting = true;
    packetPendingIV = true;
  }

  bool get isTaskHoldingOrWaiting =>
      taskHolding || (!packetPendingIV && taskWaiting);
  bool get isWaitingWithPacket =>
      packetPendingIV && taskWaiting && !taskHolding;

  static TaskState running() => new TaskState()..becomeRunning();
  static TaskState waiting() => new TaskState()..becomeWaiting();
  static TaskState waitingWithPacket() =>
      new TaskState()..becomeWaitingWithPacket();
}

class TaskControlBlock extends TaskState {
  TaskControlBlock link;
  int identity;
  int priority;
  Packet input;
  var handle; // the per-task data record
  RichardsBenchmark scheduler;

  TaskControlBlock(this.link, this.identity, this.priority, this.input,
      TaskState state, this.scheduler, this.handle) {
    packetPendingIV = state.packetPendingIV;
    taskWaiting = state.taskWaiting;
    taskHolding = state.taskHolding;
  }

  TaskControlBlock addInputCheckPriority(Packet packet, TaskControlBlock oldTask) {
    if (input == null) {
      input = packet;
      packetPendingIV = true;
      if (priority > oldTask.priority) return this;
    } else {
      input = append(packet, input);
    }
    return oldTask;
  }

  TaskControlBlock runTask() {
    var message = null;
    if (isWaitingWithPacket) {
      message = input;
      input = message.link;
      if (input == null) {
        becomeRunning();
      } else {
        becomePacketPending();
      }
    }
    return processWork(message);
  }

  TaskControlBlock processWork(Packet work) {
    throw new StateError('TaskControlBlock subclasses must override');
  }
}

class IdleTaskDataRecord {
  int control = 1;
  int count = 10000;
}

class WorkerTaskDataRecord {
  int destination = HANDLER_A;
  int count = 0;
}

class HandlerTaskDataRecord {
  Packet workIn;
  Packet deviceIn;
  void workInAdd(Packet p) {
    workIn = append(p, workIn);
  }

  void deviceInAdd(Packet p) {
    deviceIn = append(p, deviceIn);
  }
}

class DeviceTaskDataRecord {
  Packet pending;
}

class IdleTask extends TaskControlBlock {
  IdleTask(link, identity, priority, input, state, scheduler, handle)
      : super(link, identity, priority, input, state, scheduler, handle);
  TaskControlBlock processWork(Packet work) {
    IdleTaskDataRecord data = handle;
    data.count = data.count - 1;
    if (data.count == 0) return scheduler.holdSelf();
    var control = data.control;
    if ((control & 1) == 0) {
      data.control = control ~/ 2;
      return scheduler.release(DEVICE_A);
    } else {
      data.control = (control ~/ 2) ^ 53256;
      return scheduler.release(DEVICE_B);
    }
  }
}

class WorkerTask extends TaskControlBlock {
  WorkerTask(link, identity, priority, input, state, scheduler, handle)
      : super(link, identity, priority, input, state, scheduler, handle);
  TaskControlBlock processWork(Packet work) {
    WorkerTaskDataRecord data = handle;
    if (work == null) return scheduler.markWaiting();
    data.destination =
        (data.destination == HANDLER_A) ? HANDLER_B : HANDLER_A;
    work.identity = data.destination;
    work.datum = 1;
    for (var i = 1; i <= 4; i++) {
      data.count = data.count + 1;
      if (data.count > 26) data.count = 1;
      work.data[i] = 65 + data.count - 1;
    }
    return scheduler.queuePacket(work);
  }
}

class HandlerTask extends TaskControlBlock {
  HandlerTask(link, identity, priority, input, state, scheduler, handle)
      : super(link, identity, priority, input, state, scheduler, handle);
  TaskControlBlock processWork(Packet work) {
    HandlerTaskDataRecord data = handle;
    if (work != null) {
      if (work.kind == WORK_PACKET_KIND) {
        data.workInAdd(work);
      } else {
        data.deviceInAdd(work);
      }
    }
    var workPacket = data.workIn;
    if (workPacket == null) return scheduler.markWaiting();
    var count = workPacket.datum;
    if (count > 4) {
      data.workIn = workPacket.link;
      return scheduler.queuePacket(workPacket);
    }
    var devicePacket = data.deviceIn;
    if (devicePacket == null) return scheduler.markWaiting();
    data.deviceIn = devicePacket.link;
    devicePacket.datum = workPacket.data[count];
    workPacket.datum = count + 1;
    return scheduler.queuePacket(devicePacket);
  }
}

class DeviceTask extends TaskControlBlock {
  DeviceTask(link, identity, priority, input, state, scheduler, handle)
      : super(link, identity, priority, input, state, scheduler, handle);
  TaskControlBlock processWork(Packet work) {
    DeviceTaskDataRecord data = handle;
    if (work == null) {
      var functionWork = data.pending;
      if (functionWork == null) return scheduler.markWaiting();
      data.pending = null;
      return scheduler.queuePacket(functionWork);
    } else {
      data.pending = work;
      return scheduler.holdSelf();
    }
  }
}

class RichardsBenchmark {
  TaskControlBlock taskList;
  TaskControlBlock currentTask;
  int currentTaskIdentity = 0;
  List taskTable = new List(7); // 1-based, identities 1..6
  int queuePacketCount = 0;
  int holdCount = 0;

  void addTask(TaskControlBlock aTask, int identity) {
    taskList = aTask;
    taskTable[identity] = aTask;
  }

  TaskControlBlock findTask(int identity) {
    TaskControlBlock t = taskTable[identity];
    if (t == null) throw new StateError('findTask failed');
    return t;
  }

  TaskControlBlock holdSelf() {
    holdCount = holdCount + 1;
    currentTask.taskHolding = true;
    return currentTask.link;
  }

  TaskControlBlock queuePacket(Packet packet) {
    var t = findTask(packet.identity);
    queuePacketCount = queuePacketCount + 1;
    packet.link = null;
    packet.identity = currentTaskIdentity;
    return t.addInputCheckPriority(packet, currentTask);
  }

  TaskControlBlock release(int identity) {
    var t = findTask(identity);
    t.taskHolding = false;
    if (t.priority > currentTask.priority) return t;
    return currentTask;
  }

  TaskControlBlock markWaiting() {
    currentTask.taskWaiting = true;
    return currentTask;
  }

  void schedule() {
    currentTask = taskList;
    while (currentTask != null) {
      if (currentTask.isTaskHoldingOrWaiting) {
        currentTask = currentTask.link;
      } else {
        currentTaskIdentity = currentTask.identity;
        currentTask = currentTask.runTask();
      }
    }
  }

  int start() {
    addTask(
        new IdleTask(taskList, IDLER_ID, 0, null, TaskState.running(), this,
            new IdleTaskDataRecord()),
        IDLER_ID);

    var workQ = new Packet(null, WORKER_ID, WORK_PACKET_KIND);
    workQ = new Packet(workQ, WORKER_ID, WORK_PACKET_KIND);
    addTask(
        new WorkerTask(taskList, WORKER_ID, 1000, workQ,
            TaskState.waitingWithPacket(), this, new WorkerTaskDataRecord()),
        WORKER_ID);

    workQ = new Packet(null, DEVICE_A, DEVICE_PACKET_KIND);
    workQ = new Packet(workQ, DEVICE_A, DEVICE_PACKET_KIND);
    workQ = new Packet(workQ, DEVICE_A, DEVICE_PACKET_KIND);
    addTask(
        new HandlerTask(taskList, HANDLER_A, 2000, workQ,
            TaskState.waitingWithPacket(), this, new HandlerTaskDataRecord()),
        HANDLER_A);

    workQ = new Packet(null, DEVICE_B, DEVICE_PACKET_KIND);
    workQ = new Packet(workQ, DEVICE_B, DEVICE_PACKET_KIND);
    workQ = new Packet(workQ, DEVICE_B, DEVICE_PACKET_KIND);
    addTask(
        new HandlerTask(taskList, HANDLER_B, 3000, workQ,
            TaskState.waitingWithPacket(), this, new HandlerTaskDataRecord()),
        HANDLER_B);

    addTask(
        new DeviceTask(taskList, DEVICE_A, 4000, null, TaskState.waiting(),
            this, new DeviceTaskDataRecord()),
        DEVICE_A);
    addTask(
        new DeviceTask(taskList, DEVICE_B, 5000, null, TaskState.waiting(),
            this, new DeviceTaskDataRecord()),
        DEVICE_B);

    schedule();

    return (queuePacketCount * 100000) + holdCount;
  }
}

int benchRichards() {
  var r = new RichardsBenchmark().start();
  if (r != 2324609297) throw new StateError('richards: wrong result');
  return r;
}

// ═══ DeltaBlue (world/41a lines 469-1036, class-for-class) ════════════

class Strength {
  final String symbolicValue;
  final int arithmeticValue;
  Strength(this.symbolicValue, this.arithmeticValue);

  static Map table;
  static Strength required_,
      strongPreferred,
      preferred,
      strongDefault,
      normal,
      weakDefault,
      weakest;

  static Strength addStrength(String sym, int n) {
    var s = new Strength(sym, n);
    table[sym] = s;
    return s;
  }

  static void initStrengths() {
    table = new Map();
    required_ = addStrength('required', 0);
    strongPreferred = addStrength('strongPreferred', 1);
    preferred = addStrength('preferred', 2);
    strongDefault = addStrength('strongDefault', 3);
    normal = addStrength('normal', 4);
    weakDefault = addStrength('weakDefault', 5);
    weakest = addStrength('weakest', 6);
  }

  static Strength of(String sym) => table[sym];

  bool sameAs(Strength s) => arithmeticValue == s.arithmeticValue;
  bool stronger(Strength s) => arithmeticValue < s.arithmeticValue;
  bool weaker(Strength s) => arithmeticValue > s.arithmeticValue;
  Strength weakestOf(Strength s) => weaker(s) ? this : s;
}

class Variable {
  int value = 0;
  List constraints = new List();
  AbstractConstraint determinedBy;
  Strength walkStrength = Strength.weakest;
  bool stay = true;
  int mark = 0;

  Variable();
  Variable.withValue(this.value);

  void addConstraint(AbstractConstraint c) {
    constraints.add(c);
  }

  // The .mst rebuilds without the element (its OrderedCollection has no
  // remove:); List.remove is behaviorally identical here (short lists,
  // off the hot path).
  void removeConstraint(AbstractConstraint c) {
    constraints.remove(c);
    if (identical(determinedBy, c)) determinedBy = null;
  }
}

class Plan {
  List list = new List();
  void addLast(AbstractConstraint c) {
    list.add(c);
  }

  void execute() {
    for (var i = 0; i < list.length; i++) list[i].execute();
  }
}

class Planner {
  int currentMark = 0;
  static Planner current;
  static void reset() {
    current = new Planner();
  }

  int newMark() {
    currentMark = currentMark + 1;
    return currentMark;
  }

  void incrementalAdd(AbstractConstraint c) {
    var mark = newMark();
    var overridden = c.satisfy(mark);
    while (overridden != null) {
      overridden = overridden.satisfy(mark);
    }
  }

  void incrementalRemove(AbstractConstraint c) {
    var out = c.output;
    c.markUnsatisfied();
    c.removeFromGraph();
    var unsatisfied = removePropagateFrom(out);
    for (var i = 0; i < unsatisfied.length; i++) {
      incrementalAdd(unsatisfied[i]);
    }
  }

  bool addPropagate(AbstractConstraint aConstraint, int mark) {
    var todo = new List();
    todo.add(aConstraint);
    while (todo.length > 0) {
      AbstractConstraint d = todo.removeAt(0);
      if (d.output.mark == mark) {
        incrementalRemove(aConstraint);
        return false;
      }
      d.recalculate();
      addConstraintsConsumingTo(d.output, todo);
    }
    return true;
  }

  List removePropagateFrom(Variable out) {
    out.determinedBy = null;
    out.walkStrength = Strength.weakest;
    out.stay = true;
    var unsatisfied = new List();
    var todo = new List();
    todo.add(out);
    while (todo.length > 0) {
      Variable v = todo.removeAt(0);
      for (var i = 0; i < v.constraints.length; i++) {
        AbstractConstraint c = v.constraints[i];
        if (!c.isSatisfied) unsatisfied.add(c);
      }
      constraintsConsumingDo(v, (AbstractConstraint c) {
        c.recalculate();
        todo.add(c.output);
      });
    }
    return unsatisfied;
  }

  Plan extractPlanFromConstraints(List constraints) {
    var sources = new List();
    for (var i = 0; i < constraints.length; i++) {
      AbstractConstraint c = constraints[i];
      if (c.isInput && c.isSatisfied) sources.add(c);
    }
    return makePlan(sources);
  }

  Plan makePlan(List sources) {
    var mark = newMark();
    var plan = new Plan();
    var todo = sources;
    while (todo.length > 0) {
      AbstractConstraint c = todo.removeAt(0);
      if (c.output.mark != mark && c.inputsKnown(mark)) {
        plan.addLast(c);
        c.output.mark = mark;
        addConstraintsConsumingTo(c.output, todo);
      }
    }
    return plan;
  }

  void constraintsConsumingDo(Variable v, void action(AbstractConstraint c)) {
    var determining = v.determinedBy;
    for (var i = 0; i < v.constraints.length; i++) {
      AbstractConstraint c = v.constraints[i];
      if (!identical(c, determining) && c.isSatisfied) action(c);
    }
  }

  void addConstraintsConsumingTo(Variable v, List coll) {
    constraintsConsumingDo(v, (AbstractConstraint c) {
      coll.add(c);
    });
  }
}

abstract class AbstractConstraint {
  Strength strength;

  bool get isInput => false;
  bool get isSatisfied;
  Variable get output;

  void addConstraint() {
    addToGraph();
    Planner.current.incrementalAdd(this);
  }

  void destroyConstraint() {
    if (isSatisfied) {
      Planner.current.incrementalRemove(this);
    } else {
      removeFromGraph();
    }
  }

  AbstractConstraint satisfy(int mark) {
    chooseMethod(mark);
    if (!isSatisfied) {
      if (strength.sameAs(Strength.required_)) {
        throw new StateError('Could not satisfy a required constraint');
      }
      return null;
    }
    markInputs(mark);
    var out = output;
    var overridden = out.determinedBy;
    if (overridden != null) overridden.markUnsatisfied();
    out.determinedBy = this;
    if (!Planner.current.addPropagate(this, mark)) {
      throw new StateError('Cycle encountered');
    }
    out.mark = mark;
    return overridden;
  }

  void markInputs(int mark) {
    inputsDo((Variable v) {
      v.mark = mark;
    });
  }

  bool inputsKnown(int mark) {
    return !inputsHasOne((Variable v) =>
        !(v.mark == mark || v.stay || v.determinedBy == null));
  }

  void addToGraph();
  void removeFromGraph();
  void chooseMethod(int mark);
  void inputsDo(void action(Variable v));
  bool inputsHasOne(bool test(Variable v));
  void markUnsatisfied();
  void recalculate();
  void execute();
}

class UnaryConstraint extends AbstractConstraint {
  Variable myOutput;
  bool satisfied = false;

  UnaryConstraint(Variable v, String strengthSym) {
    strength = Strength.of(strengthSym);
    myOutput = v;
    satisfied = false;
    addConstraint();
  }

  bool get isSatisfied => satisfied;
  Variable get output => myOutput;

  void addToGraph() {
    myOutput.addConstraint(this);
    satisfied = false;
  }

  void removeFromGraph() {
    if (myOutput != null) myOutput.removeConstraint(this);
    satisfied = false;
  }

  void chooseMethod(int mark) {
    satisfied = (myOutput.mark != mark) && strength.stronger(myOutput.walkStrength);
  }

  void inputsDo(void action(Variable v)) {}
  bool inputsHasOne(bool test(Variable v)) => false;

  void markUnsatisfied() {
    satisfied = false;
  }

  void recalculate() {
    myOutput.walkStrength = strength;
    myOutput.stay = !isInput;
    if (myOutput.stay) execute();
  }

  void execute() {}
}

class StayConstraint extends UnaryConstraint {
  StayConstraint(Variable v, String s) : super(v, s);
}

class EditConstraint extends UnaryConstraint {
  EditConstraint(Variable v, String s) : super(v, s);
  bool get isInput => true;
}

const int FORWARD = 1;
const int BACKWARD = 2;

class BinaryConstraint extends AbstractConstraint {
  Variable v1;
  Variable v2;
  int direction; // null = unsatisfied, mirrors the .mst's nil/#forward/#backward

  BinaryConstraint.noInit();

  BinaryConstraint(Variable a, Variable b, String strengthSym) {
    strength = Strength.of(strengthSym);
    v1 = a;
    v2 = b;
    direction = null;
    addConstraint();
  }

  bool get isSatisfied => direction != null;

  void addToGraph() {
    v1.addConstraint(this);
    v2.addConstraint(this);
    direction = null;
  }

  void removeFromGraph() {
    if (v1 != null) v1.removeConstraint(this);
    if (v2 != null) v2.removeConstraint(this);
    direction = null;
  }

  void chooseMethod(int mark) {
    if (v1.mark == mark) {
      direction = (v2.mark != mark && strength.stronger(v2.walkStrength))
          ? FORWARD
          : null;
      return;
    }
    if (v2.mark == mark) {
      direction = (v1.mark != mark && strength.stronger(v1.walkStrength))
          ? BACKWARD
          : null;
      return;
    }
    // Neither variable is marked: prefer to flow toward the weaker
    // walkabout strength.
    if (v1.walkStrength.weaker(v2.walkStrength)) {
      direction = strength.stronger(v1.walkStrength) ? BACKWARD : null;
    } else {
      direction = strength.stronger(v2.walkStrength) ? FORWARD : null;
    }
  }

  void inputsDo(void action(Variable v)) {
    if (direction == FORWARD) {
      action(v1);
    } else {
      action(v2);
    }
  }

  bool inputsHasOne(bool test(Variable v)) {
    return direction == FORWARD ? test(v1) : test(v2);
  }

  void markUnsatisfied() {
    direction = null;
  }

  Variable get output => direction == FORWARD ? v2 : v1;

  void recalculate() {
    Variable inV, outV;
    if (direction == FORWARD) {
      inV = v1;
      outV = v2;
    } else {
      inV = v2;
      outV = v1;
    }
    outV.walkStrength = strength.weakestOf(inV.walkStrength);
    outV.stay = inV.stay;
    if (outV.stay) execute();
  }

  void execute() {}
}

class EqualityConstraint extends BinaryConstraint {
  EqualityConstraint(Variable a, Variable b, String s) : super(a, b, s);
  void execute() {
    if (direction == FORWARD) {
      v2.value = v1.value;
    } else {
      v1.value = v2.value;
    }
  }
}

class ScaleConstraint extends BinaryConstraint {
  Variable scale;
  Variable offset;

  ScaleConstraint(Variable src, Variable scaleV, Variable offsetV,
      Variable dst, String strengthSym)
      : super.noInit() {
    strength = Strength.of(strengthSym);
    v1 = src;
    v2 = dst;
    scale = scaleV;
    offset = offsetV;
    direction = null;
    addConstraint();
  }

  void addToGraph() {
    v1.addConstraint(this);
    v2.addConstraint(this);
    scale.addConstraint(this);
    offset.addConstraint(this);
    direction = null;
  }

  void removeFromGraph() {
    if (v1 != null) v1.removeConstraint(this);
    if (v2 != null) v2.removeConstraint(this);
    if (scale != null) scale.removeConstraint(this);
    if (offset != null) offset.removeConstraint(this);
    direction = null;
  }

  void inputsDo(void action(Variable v)) {
    if (direction == FORWARD) {
      action(v1);
    } else {
      action(v2);
    }
    action(scale);
    action(offset);
  }

  bool inputsHasOne(bool test(Variable v)) {
    if (direction == FORWARD) {
      if (test(v1)) return true;
    } else {
      if (test(v2)) return true;
    }
    return test(scale) || test(offset);
  }

  void execute() {
    if (direction == FORWARD) {
      v2.value = v1.value * scale.value + offset.value;
    } else {
      v1.value = (v2.value - offset.value) ~/ scale.value;
    }
  }

  void recalculate() {
    Variable inV, outV;
    if (direction == FORWARD) {
      inV = v1;
      outV = v2;
    } else {
      inV = v2;
      outV = v1;
    }
    outV.walkStrength = strength.weakestOf(inV.walkStrength);
    outV.stay = inV.stay && scale.stay && offset.stay;
    if (outV.stay) execute();
  }
}

int chainTest(int n) {
  Planner.reset();
  Variable prev, v, first, last;
  prev = null;
  for (var i = 1; i <= n + 1; i++) {
    v = new Variable();
    if (prev != null) new EqualityConstraint(prev, v, 'required');
    if (i == 1) first = v;
    if (i == n + 1) last = v;
    prev = v;
  }
  new StayConstraint(last, 'strongDefault');
  var editC = new EditConstraint(first, 'preferred');
  var edits = new List();
  edits.add(editC);
  var plan = Planner.current.extractPlanFromConstraints(edits);
  for (var i = 0; i <= 99; i++) {
    first.value = i;
    plan.execute();
    if (last.value != i) throw new StateError('Chain test failed');
  }
  return last.value;
}

void change(Variable aVariable, int newValue) {
  var editC = new EditConstraint(aVariable, 'preferred');
  var edits = new List();
  edits.add(editC);
  var plan = Planner.current.extractPlanFromConstraints(edits);
  for (var r = 0; r < 10; r++) {
    aVariable.value = newValue;
    plan.execute();
  }
  editC.destroyConstraint();
}

int projectionTest(int n) {
  Planner.reset();
  var dests = new List();
  var scale = new Variable.withValue(10);
  var offset = new Variable.withValue(1000);
  Variable src, dst;
  for (var i = 1; i <= n; i++) {
    src = new Variable.withValue(i);
    dst = new Variable.withValue(i);
    dests.add(dst);
    new StayConstraint(src, 'normal');
    new ScaleConstraint(src, scale, offset, dst, 'required');
  }
  change(src, 17);
  if (dst.value != 1170) throw new StateError('Projection test 1 failed');
  change(dst, 1050);
  if (src.value != 5) throw new StateError('Projection test 2 failed');
  change(scale, 5);
  for (var i = 1; i <= n - 1; i++) {
    if (dests[i - 1].value != i * 5 + 1000) {
      throw new StateError('Projection test 3 failed');
    }
  }
  change(offset, 2000);
  var total = 0;
  for (var i = 1; i <= n - 1; i++) {
    if (dests[i - 1].value != i * 5 + 2000) {
      throw new StateError('Projection test 4 failed');
    }
    total = total + dests[i - 1].value;
  }
  return total + dst.value;
}

int benchDeltaBlue() {
  var r = chainTest(100) + projectionTest(100);
  if (r != 224874) throw new StateError('deltablue: wrong result');
  return r;
}

// ═══ harness (mirrors CogRun in scripts/cog-bench.mst exactly) ═════════

int median(List times) {
  var s = new List.from(times);
  s.sort();
  return s[((s.length + 1) ~/ 2) - 1]; // 1-based (n+1)//2, 0-indexed
}

void check(String name, got, want) {
  if (got != want) {
    print('$name WRONG RESULT $got want $want');
    exit(1);
  }
}

void run(String name, Function block, want) {
  var sw = new Stopwatch()..start();
  var r;
  for (var i = 0; i < 10; i++) r = block();
  var cold = sw.elapsedMicroseconds;
  check(name, r, want);
  var times = new List();
  for (var b = 0; b < 6; b++) {
    sw
      ..reset()
      ..start();
    for (var i = 0; i < 10; i++) r = block();
    times.add(sw.elapsedMicroseconds);
  }
  check(name, r, want);
  print('$name cold_us=$cold warm_us=${median(times)}');
}

// With no argument, runs the whole suite in one process (matches the
// Smalltalk harnesses). With a bench name argument, runs ONLY that bench —
// used by dart-bench.sh to give each workload its own process with the
// flags the 2017 arm64 JIT needs for it (two distinct optimizer corners
// SIGILL on this vintage: the background-compiler install race, and the
// polymorphic-with-deopt / megamorphic paths — see docs/cog_bench.md).
void main(List<String> args) {
  Strength.initStrengths();
  var all = {
    'arith': () => run('arith    ', benchArith, 1124997749998000000),
    'fib': () => run('fib      ', benchFib, 2178309),
    'sieve': () => run('sieve    ', benchSieve, 1899),
    'dict': () => run('dict     ', benchDict, 170698668000),
    'alloc': () => run('alloc    ', benchAlloc, 200000),
    'richards': () => run('richards ', benchRichards, 2324609297),
    'deltablue': () => run('deltablue', benchDeltaBlue, 224874),
  };
  if (args.length > 0) {
    var f = all[args[0]];
    if (f == null) {
      print('unknown bench ${args[0]}');
      exit(2);
    }
    f();
    return;
  }
  all['arith']();
  all['fib']();
  all['sieve']();
  all['dict']();
  all['alloc']();
  all['richards']();
  all['deltablue']();
}
