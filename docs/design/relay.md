# relay (없어진 설계)

iroh-relay를 쓰던 relay 설계는 없어졌다(사용자 결정 2026-11-27, P2P에서 iroh를 쓰지 않는다). 새 구조는 데이터를 중계하지 않는 rendezvous 서버다. 설계는 [rendezvous](rendezvous.md)와 [P2P](p2p.md)에 있다.

이 파일은 아직 develop에 남아 있는 `relay/` 패키지의 문서가 이 경로를 가리키기 때문에 남겨 둔 안내다. `relay/` 패키지를 없애는 작업([rendezvous](rendezvous.md#relay-패키지와-relayyml의-처리-방침))에서 이 파일도 지운다.
