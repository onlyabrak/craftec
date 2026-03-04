# Craftec Technical Foundation v3.3 — Errata

Corrections needed for v3.4. These items contain Reed-Solomon or BitTorrent concepts
that were incorrectly applied to the RLNC-based design.

---

## §29 Piece Distribution (page 51)

### E1: "Rarest-first selection" is an RS/BitTorrent concept
- **Current**: "Distributes coded pieces to peers. Rarest-first selection, end-game mode, batching, retry with backoff."
- **Problem**: RLNC coded pieces are ALL unique — each has a random GF(2^8) coefficient vector. There is no concept of "rare" vs "common" pieces. Rarest-first is a BitTorrent strategy for RS-style identical-piece systems.
- **Correction**: "Distributes coded pieces to peers. Distributes to non-holders first (peers with 0 pieces), then under-holders (peers with 1 piece to bring them to ≥2 for future repair eligibility). Retry with backoff."

### E2: "End-game mode" is an RS/BitTorrent concept
- **Current**: "End-game mode: when ≥(n-2) delivered, send last 2 to all remaining peers."
- **Problem**: End-game mode optimises the "last pieces" problem in RS/BitTorrent where specific missing pieces must be found. In RLNC, any new coded piece is useful to any peer — there are no "last 2" special pieces.
- **Correction**: Remove end-game mode entirely. RLNC distribution simply sends unique coded pieces to peers that need more. No special end-game handling needed.

### E3: "Batch 8 pieces per peer per round" implies fixed batching
- **Current**: "Batch 8 pieces per peer per round. 10s timeout per piece."
- **Problem**: Fixed batch counts come from RS thinking where pieces are interchangeable. RLNC generates n = k × ceil(2 + 16/k) total pieces per CID and distributes them across ≥K distinct peers.
- **Correction**: "Distribute n coded pieces across ≥K distinct peers. Target: each peer receives at least 1 piece, with enough diversity for any K peers to reconstruct. 10s timeout per peer."

---

## §30 Health Scanning & Repair (pages 52–53)

### E4: "piece_rank" — no piece rank in RLNC
- **Current**: "If piece_rank < target n: trigger repair via Natural Selection Coordinator."
- **Problem**: RLNC pieces don't have ranks or indices. The scanner checks the count of available pieces (nodes holding a piece for this CID), not a "rank".
- **Correction**: "If available_count < target n: trigger repair via Natural Selection Coordinator."

### E5: "fetch ≥2 coded pieces" — no network fetch for repair
- **Current**: "Repair: fetch ≥2 coded pieces, recode with fresh coefficients (no decode needed), distribute 1 piece."
- **Problem**: Implies the repair node must fetch pieces from the network. A repair node already holds ≥2 pieces locally (that's the eligibility requirement). Recoding happens from local pieces only — no network fetch needed.
- **Correction**: "Repair: recode from ≥2 locally-held coded pieces with fresh GF(2^8) coefficients (no decode needed), distribute 1 new piece to a peer lacking pieces."

### E6: Scan scope says "≥1 piece" — should be "≥2 pieces"
- **Current**: "Scope: Locally-held CIDs only (this node holds ≥1 piece for each scanned CID)."
- **Problem**: A node needs ≥2 pieces to recode. Nodes with only 1 piece cannot participate in repair, so scanning those CIDs wastes cycles. Scanning should be limited to CIDs where the node can actually act.
- **Correction**: "Scope: Locally-held CIDs only (this node holds ≥2 pieces for each scanned CID — the minimum required to recode)."

### E7: Candidate pool says "≥1 piece" — should be "≥2 pieces"
- **Current**: "Candidate pool: All provider nodes (nodes holding ≥1 piece for the CID) form the candidate coordinator pool."
- **Problem**: Same as E6. A node with only 1 piece cannot recode and therefore cannot be a repair coordinator.
- **Correction**: "Candidate pool: All provider nodes holding ≥2 pieces for the CID form the candidate repair pool."

### E8: Single coordinator model — should be parallel top-N
- **Current**: "Top-ranked node acts: The top-ranked node acts as repair coordinator for that CID in the current cycle. No explicit election needed."
- **Problem**: With a single coordinator, repair of large deficits (e.g., deficit = 10) takes 10 cycles. The correct design elects the top-N nodes (where N = deficit), each producing 1 new piece per cycle. This is parallel repair.
- **Correction**: "Top-N nodes act: When deficit = N pieces needed, the top-N ranked nodes each independently produce 1 new coded piece per cycle. Each node deterministically computes the same ranking and knows if it's in the top-N. No explicit election or coordination needed."

### E9: "1 piece per CID per cycle" — missing per-node clarification
- **Current**: "1 piece per CID per cycle: Each coordinator repairs or degrades only 1 piece per CID per cycle. Prevents repair storms."
- **Problem**: This reads as if only 1 piece is produced network-wide per cycle. With top-N parallel repair, N pieces are produced per cycle (1 per elected node).
- **Correction**: "1 piece per elected node per CID per cycle: Each of the top-N elected nodes produces 1 piece per CID per cycle. Total repair rate = min(N, deficit) pieces per cycle. Prevents repair storms while still converging quickly."

---

## §55 Repair Path (page 85)

### E10: Flow header includes "Fetch Pieces"
- **Current**: "Health Scan → Detect Under-Replication → Fetch Pieces → Recode → Distribute"
- **Correction**: "Health Scan → Detect Under-Replication → Local Recode → Distribute"

### E11: Step 3 uses "piece rank"
- **Current**: "Compute piece rank. If < K → DATA AT RISK. If < n → under-replicated."
- **Correction**: "Compute available piece count. If < K → DATA AT RISK. If < n → under-replicated."

### E12: Step 4 implies network fetch for repair
- **Current**: "Select 2+ peers holding coded pieces. Fetch via QUIC."
- **Problem**: Repair nodes already hold ≥2 pieces locally. No fetch needed.
- **Correction**: "Verify this node holds ≥2 local coded pieces. Recode from local pieces (no network fetch)."

### E13: Missing parallel repair and distribution priority
- **Problem**: §55 doesn't mention top-N parallel election or distribution target priority.
- **Add to step 4**: "Determine if this node is in the top-N elected repairers (where N = deficit). If not elected, skip."
- **Add to step 7**: "Distribute new coded piece with priority: (1) peers holding exactly 1 piece (so they reach ≥2 and become repair-eligible), (2) peers holding 0 pieces."

---

## §60 Network Partition Recovery (page 91)

### E14: "1 piece per node per cycle" — ambiguous
- **Current**: "Both network halves repair independently using the natural selection coordinator (1 piece per node per cycle)."
- **Correction**: "Both network halves repair independently using top-N natural selection (each of the top-N elected nodes produces 1 piece per CID per cycle, where N = deficit)."

---

## Summary of incorrect RS/BitTorrent concepts to remove globally

| Concept | Origin | RLNC Reality |
|---------|--------|--------------|
| Rarest-first | BitTorrent | Every RLNC piece is unique — no rarity concept |
| End-game mode | BitTorrent | Any new piece helps any peer — no "last pieces" problem |
| Piece index/rank | Reed-Solomon | RLNC pieces have coefficient vectors, not indices |
| Batch N pieces per peer | RS distribution | Distribute n = k × ceil(2+16/k) total, ≥1 per peer |
| Fetch pieces for repair | RS repair | Recode from ≥2 LOCAL pieces, no network fetch |
| Single coordinator | Centralised repair | Top-N parallel repair (N = deficit) |
| ≥1 piece eligibility | — | ≥2 pieces required to recode |
