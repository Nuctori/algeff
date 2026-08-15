------------------------------ MODULE scheduler ------------------------------
(*
   Algeff 公理 A7：无死锁调度模型（pdr.md §四 公理 A7 / §七 工程映射表）
   ==================================================================
   策略：原子占坑 + 失败回滚 + 有限重试。
     - 任务 t 一次原子尝试占取其所需资源集 requested[t]；
     - 全部空闲 -> 一举持有并进入 running，执行完毕释放全部（done）；
     - 任一被占 -> 什么都不持有（原子性本身就是"回滚"，无需部分释放），
       重试计数 +1；达到 MaxRetries -> 永久放弃（failed，abort）。
     - 因为占坑是原子的，任何可达状态都不存在"持有部分资源、等待其余
       资源"的半占状态，hold-and-wait 链无法形成 -> 无循环等待（A7）。

   不变式 NoCircularWait 定义在**通用** waits-for 图上（不依赖本策略的
   原子性假设）：若未来实现退化为"逐资源增量占坑 + 持久持有"，该不变式
   会被违反——这正是本模型要守住并交给 TLC 持续检查的断言。

   公平性：弱公平（WF）。TLC 中 WF 作为活性属性 Progress 的前提，
   不放进 Spec（保持 Spec = Init /\ [][Next]_vars 便于 Apalache 只查不变式）。
*)
EXTENDS Naturals, FiniteSets

CONSTANTS
  R,          \* 资源集合（有限）
  T,          \* 任务集合（有限）
  requested,  \* requested[t] \subseteq R：任务 t 所需资源集
  MaxRetries  \* 有限重试上限（\in Nat）

ASSUME MaxRetries \in Nat
ASSUME \A t \in T : requested[t] \subseteq R

VARIABLES
  holders,    \* holders[r] \subseteq T：持有 r 的任务集（不变式保证 |.| <= 1）
  retries,    \* retries[t]：t 累计失败尝试次数
  status      \* status[t] \in {"idle","running","done","failed"}

vars == <<holders, retries, status>>

Init ==
  /\ holders = [r \in R |-> {}]
  /\ retries = [t \in T |-> 0]
  /\ status  = [t \in T |-> "idle"]

(* 原子占坑成功：所需资源全部空闲，一举持有（单个原子动作，无中间态） *)
Claim(t) ==
  /\ status[t] = "idle"
  /\ retries[t] < MaxRetries
  /\ \A r \in requested[t] : holders[r] = {}
  /\ holders' = [r \in R |-> IF r \in requested[t] THEN {t} ELSE holders[r]]
  /\ status'  = [status EXCEPT ![t] = "running"]
  /\ UNCHANGED retries

(* 占坑失败：原子性保证此刻未持有任何资源（无需释放）；
   有限重试：计数 +1，耗尽则永久 abort（failed）。 *)
ClaimFail(t) ==
  /\ status[t] = "idle"
  /\ retries[t] < MaxRetries
  /\ \E r \in requested[t] : holders[r] /= {}
  /\ retries' = [retries EXCEPT ![t] = retries[t] + 1]
  /\ status'  = [status EXCEPT ![t] =
                     IF retries[t] + 1 >= MaxRetries THEN "failed" ELSE "idle"]
  /\ UNCHANGED holders

(* 执行完毕：释放全部占坑 *)
Finish(t) ==
  /\ status[t] = "running"
  /\ holders' = [r \in R |-> IF r \in requested[t] THEN {} ELSE holders[r]]
  /\ status'  = [status EXCEPT ![t] = "done"]
  /\ UNCHANGED retries

TryClaim(t) == Claim(t) \/ ClaimFail(t)

Next == \E t \in T : TryClaim(t) \/ Finish(t)

Spec == Init /\ [][Next]_vars

(* ============================ 不变式 ============================ *)

TypeOK ==
  /\ holders \in [R -> SUBSET T]
  /\ retries \in [T -> 0..MaxRetries]
  /\ status  \in [T -> {"idle","running","done","failed"}]

(* 互斥：任一资源至多被一个任务持有 *)
ExclusiveHold ==
  \A r \in R : Cardinality(holders[r]) <= 1

(* 运行中的任务恰好持有其所需资源集（不多不少，防止过度占坑/占坑不足） *)
ExactHold ==
  /\ \A t \in T : status[t] = "running"
      => \A r \in requested[t] : t \in holders[r]
  /\ \A t \in T : status[t] = "running"
      => \A r \in R \ requested[t] : t \notin holders[r]

(* 等待关系：t 等待 u，当 u 持有 t 所需的某个资源（排除自等待） *)
WaitsFor(t) ==
  { u \in T : u /= t /\ \E r \in requested[t] : u \in holders[r] }

(* 无循环等待（A7）：waits-for 图上不存在环。
   等价刻画：不存在非空任务子集 S，其中每个任务都等待 S 内另一任务。 *)
NoCircularWait ==
  ~\E S \in SUBSET T :
      S /= {} /\ \A t \in S : \E u \in S : u \in WaitsFor(t)

(* ============================ 活性 ============================ *)
(* 需 TLC 检查（Apalache 不支持时序算子 <>）。
   前提：TryClaim(t) 与 Finish(t) 弱公平。结论：每个任务最终
   running（随后 done）或 failed（重试耗尽 abort），不会永久停滞。 *)
Progress ==
  \A t \in T :
    (WF_vars(TryClaim(t)) /\ WF_vars(Finish(t)))
      => <> (status[t] \in {"running","done","failed"})

=============================================================================
