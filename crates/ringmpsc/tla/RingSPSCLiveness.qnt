// Quint Liveness Specification for SPSC Ring Buffer
//
// This module holds the temporal (liveness) properties for RingSPSC.qnt,
// expressible in Quint since 0.32.0 introduced the `leadsTo` operator
// (the TLA+ `~>`). It lives in a separate module so that the safety spec
// (RingSPSC.qnt) remains loadable by Quint < 0.32.0 toolchains — in
// particular the quint-connect MBT pipeline, which only needs the safety
// module.
//
// History: this property originated as `EventuallyConsumed` in the retired
// RingSPSC.tla. There it was never actually checked — the `PROPERTIES`
// entry in RingSPSC.cfg was commented out ("requires fairness assumptions"),
// and the TLA+ behavior spec `Init /\ [][Next]_vars` declared no fairness,
// so the property was unprovable as written: a behavior that stutters
// forever after producing MaxItems items violates it. The Quint port states
// the required weak-fairness assumption explicitly in the property, making
// this the first formulation that verification can actually discharge.
//
// Run with (requires Quint >= 0.32.0, JDK 21+):
//   quint verify RingSPSCLiveness.qnt --main=RingSPSCLiveness \
//       --temporal=eventuallyConsumed --backend=tlc

module RingSPSCLiveness {
    import RingSPSC.* from "./RingSPSC"

    /// Weak fairness on the consumer's advance action: if consuming is
    /// continuously enabled (hd < tl), it eventually happens. This is the
    /// assumption the TLA+ spec's prose comment gestured at but never
    /// declared formally (no WF_vars in the behavior spec).
    ///
    /// The witness variable is `hd`: consumerAdvance always changes it,
    /// so <<consumerAdvance>>_hd is exactly a non-stuttering advance.
    temporal consumerFair = weakFair(consumerAdvance, hd)

    /// EventuallyConsumed (ex RingSPSC.tla): once the producer has produced
    /// everything (items_produced == MAX_ITEMS), the consumer eventually
    /// drains the ring (hd == tl) — provided the consumer is treated fairly.
    /// Without the fairness antecedent this is unprovable: the system may
    /// stutter forever with items still in the ring.
    temporal eventuallyConsumed =
        consumerFair.implies((items_produced == MAX_ITEMS).leadsTo(hd == tl))
}
