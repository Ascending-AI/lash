//! The byte-identity law for OpenAI Responses, Chat Completions and Codex
//! (FIG-6443): every corpus history, under each request shape, lowers to
//! the body recorded before lowering became incremental.

use super::*;
use lash_protocol_types::{
    llm::wire_corpus::corpus_tool, llm::wire_corpus::digest_line, llm::wire_corpus::reuse_cases,
    llm::wire_corpus::text_only, llm::wire_corpus::wire_corpus,
};

/// Digests recorded from the coalescing lowering at de7f61e3dc.
const RECORDED: &str = "
responses/base/prefix-01 adc02d48a25fc70b560179ff
responses/instructed/prefix-01 c501106ed84a9a0574acdcc8
responses/uncached/prefix-01 c4854ba02b8fae1ff1a9793d
responses/cache-control/prefix-01 adc02d48a25fc70b560179ff
responses/base/prefix-02 57ece5a56bbf37c81094f6fa
responses/instructed/prefix-02 ff150b429a0c2dce94a07c74
responses/uncached/prefix-02 99513c669c77e2f15394fadc
responses/cache-control/prefix-02 57ece5a56bbf37c81094f6fa
responses/base/prefix-03 33ec44d0d8b899e2e8e9969e
responses/instructed/prefix-03 f81c7a8b87bf4aee75df4b66
responses/uncached/prefix-03 c50f38aed1090cb8d852481e
responses/cache-control/prefix-03 33ec44d0d8b899e2e8e9969e
responses/base/prefix-04 e3b37c567a74d87ac2926574
responses/instructed/prefix-04 4bf9dc2daf036010312fc7e4
responses/uncached/prefix-04 9aa955dada638dbb3c8ab679
responses/cache-control/prefix-04 e3b37c567a74d87ac2926574
responses/base/prefix-05 8890cc1413e0a5ff448c00a9
responses/instructed/prefix-05 02902bb3ef13c32dec8b3e9f
responses/uncached/prefix-05 648d616cc20a711603749d65
responses/cache-control/prefix-05 8890cc1413e0a5ff448c00a9
responses/base/prefix-06 95a701d07323952fdc66812d
responses/instructed/prefix-06 62d0ae46f8cf4608f35293ac
responses/uncached/prefix-06 1a4882ec8cac87159b9bb216
responses/cache-control/prefix-06 95a701d07323952fdc66812d
responses/base/prefix-07 f77e5f94b692183ce6e65bd4
responses/instructed/prefix-07 0800ed5e5349505ea52130e6
responses/uncached/prefix-07 3930204f94b1a9d70868803d
responses/cache-control/prefix-07 f77e5f94b692183ce6e65bd4
responses/base/prefix-08 1e0bd1ac25bd6c1cc5512622
responses/instructed/prefix-08 48add6bff8cb501ae5ce16a3
responses/uncached/prefix-08 cea5b6af8dcf25fc8750ee94
responses/cache-control/prefix-08 1e0bd1ac25bd6c1cc5512622
responses/base/prefix-09 802fde29086686efddaf75cf
responses/instructed/prefix-09 5d62af2e60da6d545ed1ed78
responses/uncached/prefix-09 e23cfaf4a2dd23bc3afd1bf2
responses/cache-control/prefix-09 802fde29086686efddaf75cf
responses/base/prefix-10 50701ad28e5a45a94989e761
responses/instructed/prefix-10 32333b8985a5d522b5f66638
responses/uncached/prefix-10 c47cf3276963435dbd9a32ac
responses/cache-control/prefix-10 50701ad28e5a45a94989e761
responses/base/prefix-11 4449b290c65b97031d784526
responses/instructed/prefix-11 4a88b922ce1bdf58ed8034cd
responses/uncached/prefix-11 5e9f16832ec819f45c2e91ad
responses/cache-control/prefix-11 4449b290c65b97031d784526
responses/base/prefix-12 0b583ecc8f6006eb7193ee9f
responses/instructed/prefix-12 f39e1895afbb68a374708c5e
responses/uncached/prefix-12 7d526745ad6d07745bd36d73
responses/cache-control/prefix-12 0b583ecc8f6006eb7193ee9f
responses/base/prefix-13 249bfd6b3120ed11fde44bbc
responses/instructed/prefix-13 4c0dd9b2743a64da781aec20
responses/uncached/prefix-13 19d38b1b31a6bc0602c55e0b
responses/cache-control/prefix-13 249bfd6b3120ed11fde44bbc
responses/base/prefix-14 7d0f06c312e9d9b163e90da2
responses/instructed/prefix-14 028272eadab3dc992a29e4d0
responses/uncached/prefix-14 d15154cb31dfddc4615032bd
responses/cache-control/prefix-14 7d0f06c312e9d9b163e90da2
responses/base/empty-history 5cad214b5fc1b513642c5575
responses/instructed/empty-history 93a0d71ba66fa733b5bb03be
responses/uncached/empty-history ed300246b567cfddb2b258ca
responses/cache-control/empty-history 5cad214b5fc1b513642c5575
responses/base/only-feedback 80e24e5cc1348ea51c89c7d5
responses/instructed/only-feedback 6d53edd3c2b2f1c1a9e6ede0
responses/uncached/only-feedback b37a05c39f3fe2e4c8d2c012
responses/cache-control/only-feedback 80e24e5cc1348ea51c89c7d5
responses/base/unicode 74c30c3e39ade2d1527e6b3c
responses/instructed/unicode 9a5fb6b07d6a1ad7753f23cb
responses/uncached/unicode 23386f2a8c96ed80fe74c46e
responses/cache-control/unicode 74c30c3e39ade2d1527e6b3c
chat/base/prefix-01 9b7ac148706f7397c0236ec4
chat/instructed/prefix-01 3c5efcefbd071620530b8312
chat/uncached/prefix-01 9b7ac148706f7397c0236ec4
chat/cache-control/prefix-01 e540e64ff683d610e2b9f872
chat/base/prefix-02 f308b1c9b5b29660388fe945
chat/instructed/prefix-02 05ad04d0e97d6e26daa97d1f
chat/uncached/prefix-02 f308b1c9b5b29660388fe945
chat/cache-control/prefix-02 03f9d31f5732bfe9a9063f8c
chat/base/prefix-03 ac3902788e85a191ea5edbef
chat/instructed/prefix-03 84b25cc154c36dcc0cf9cc56
chat/uncached/prefix-03 ac3902788e85a191ea5edbef
chat/cache-control/prefix-03 29df51929f94e393a8e62da3
chat/base/prefix-04 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-04 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-04 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-04 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-05 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-05 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-05 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-05 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-06 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-06 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-06 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-06 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-07 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-07 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-07 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-07 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-08 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-08 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-08 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-08 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-09 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-09 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-09 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-09 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-10 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-10 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-10 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-10 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-11 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-11 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-11 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-11 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-12 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-12 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-12 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-12 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-13 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-13 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-13 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-13 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/prefix-14 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/instructed/prefix-14 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/uncached/prefix-14 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/cache-control/prefix-14 refused: openai-compatible cannot encode attachment `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` MIME `image/png` at ToolResult; accepting providers: openai, codex, openai-compatible
chat/base/empty-history cf8018beaba3cd7eb043ca14
chat/instructed/empty-history 538790a95f999a462c68ed81
chat/uncached/empty-history cf8018beaba3cd7eb043ca14
chat/cache-control/empty-history cf8018beaba3cd7eb043ca14
chat/base/only-feedback 878ff4f33e19b29b73b9426a
chat/instructed/only-feedback ec3e1fd53a2f90aba0071960
chat/uncached/only-feedback 878ff4f33e19b29b73b9426a
chat/cache-control/only-feedback 99d91c15b0fd0c38395447a0
chat/base/unicode 65d4d35be5fc3581ac141c4e
chat/instructed/unicode 14dca4dd4b1155c6c8d59388
chat/uncached/unicode 65d4d35be5fc3581ac141c4e
chat/cache-control/unicode 1316ce19b9969cdad8063635
chat-text/base/prefix-01 9b7ac148706f7397c0236ec4
chat-text/instructed/prefix-01 3c5efcefbd071620530b8312
chat-text/uncached/prefix-01 9b7ac148706f7397c0236ec4
chat-text/cache-control/prefix-01 e540e64ff683d610e2b9f872
chat-text/base/prefix-02 f308b1c9b5b29660388fe945
chat-text/instructed/prefix-02 05ad04d0e97d6e26daa97d1f
chat-text/uncached/prefix-02 f308b1c9b5b29660388fe945
chat-text/cache-control/prefix-02 03f9d31f5732bfe9a9063f8c
chat-text/base/prefix-03 ac3902788e85a191ea5edbef
chat-text/instructed/prefix-03 84b25cc154c36dcc0cf9cc56
chat-text/uncached/prefix-03 ac3902788e85a191ea5edbef
chat-text/cache-control/prefix-03 29df51929f94e393a8e62da3
chat-text/base/prefix-04 4d0f1697ea4e4088ef1b5573
chat-text/instructed/prefix-04 cb72b0fb7c17d4cad2f64ec9
chat-text/uncached/prefix-04 4d0f1697ea4e4088ef1b5573
chat-text/cache-control/prefix-04 03564e1249d580d977148920
chat-text/base/prefix-05 4d0f1697ea4e4088ef1b5573
chat-text/instructed/prefix-05 cb72b0fb7c17d4cad2f64ec9
chat-text/uncached/prefix-05 4d0f1697ea4e4088ef1b5573
chat-text/cache-control/prefix-05 03564e1249d580d977148920
chat-text/base/prefix-06 ea9e076418b405ce94ff1833
chat-text/instructed/prefix-06 51df0a120bf5cd4822514f6d
chat-text/uncached/prefix-06 ea9e076418b405ce94ff1833
chat-text/cache-control/prefix-06 4ba0d239891a2195734d610a
chat-text/base/prefix-07 399d0303e42b1512e5a840a1
chat-text/instructed/prefix-07 d5bd6045274b5394d2f574e2
chat-text/uncached/prefix-07 399d0303e42b1512e5a840a1
chat-text/cache-control/prefix-07 c5726e618532cc7205c118fc
chat-text/base/prefix-08 511b1d78cf35c3deb5675a41
chat-text/instructed/prefix-08 8f3cd54d6ddcc3e3f4ab85e7
chat-text/uncached/prefix-08 511b1d78cf35c3deb5675a41
chat-text/cache-control/prefix-08 e15b8250543d02b862906ff3
chat-text/base/prefix-09 852627d4253a1bf8bfc54ca6
chat-text/instructed/prefix-09 b791752b3daa9e5f08db584d
chat-text/uncached/prefix-09 852627d4253a1bf8bfc54ca6
chat-text/cache-control/prefix-09 0dd362908287d3e8a2b00217
chat-text/base/prefix-10 fea09cd156120c7a87806177
chat-text/instructed/prefix-10 b3d655c39b10350716a966e0
chat-text/uncached/prefix-10 fea09cd156120c7a87806177
chat-text/cache-control/prefix-10 6ed2a1a800e2047eeeac5993
chat-text/base/prefix-11 6edde87c20ceed7bf30a6f09
chat-text/instructed/prefix-11 1864b2f3cb0b1aa2fcbac51b
chat-text/uncached/prefix-11 6edde87c20ceed7bf30a6f09
chat-text/cache-control/prefix-11 f1d0f28a512a313b59f50ca5
chat-text/base/prefix-12 ce51352ea7a9097e55ac8ff3
chat-text/instructed/prefix-12 941f8596d7d3578e7f273077
chat-text/uncached/prefix-12 ce51352ea7a9097e55ac8ff3
chat-text/cache-control/prefix-12 8ebc6ff85d49b3e4c053a5f2
chat-text/base/prefix-13 3101d6e2a7c4575d169e6e27
chat-text/instructed/prefix-13 e5acbd3cd995de18cefdff64
chat-text/uncached/prefix-13 3101d6e2a7c4575d169e6e27
chat-text/cache-control/prefix-13 fbb9bc94258c772cb6be898a
chat-text/base/prefix-14 edf7f46600ce1b21c9e52337
chat-text/instructed/prefix-14 2f21b0ad03a5f326dd19e4c6
chat-text/uncached/prefix-14 edf7f46600ce1b21c9e52337
chat-text/cache-control/prefix-14 935c78a45405ebef5cbe4dc9
chat-text/base/empty-history cf8018beaba3cd7eb043ca14
chat-text/instructed/empty-history 538790a95f999a462c68ed81
chat-text/uncached/empty-history cf8018beaba3cd7eb043ca14
chat-text/cache-control/empty-history cf8018beaba3cd7eb043ca14
chat-text/base/only-feedback 878ff4f33e19b29b73b9426a
chat-text/instructed/only-feedback ec3e1fd53a2f90aba0071960
chat-text/uncached/only-feedback 878ff4f33e19b29b73b9426a
chat-text/cache-control/only-feedback 99d91c15b0fd0c38395447a0
chat-text/base/unicode 65d4d35be5fc3581ac141c4e
chat-text/instructed/unicode 14dca4dd4b1155c6c8d59388
chat-text/uncached/unicode 65d4d35be5fc3581ac141c4e
chat-text/cache-control/unicode 1316ce19b9969cdad8063635
codex/base/prefix-01 849c51e99d54f7ba86dfebd1
codex/instructed/prefix-01 38a2291145b712d71653b648
codex/uncached/prefix-01 02ddab30789360e72473feb4
codex/cache-control/prefix-01 849c51e99d54f7ba86dfebd1
codex/base/prefix-02 c2deb34800efe1c63b380c20
codex/instructed/prefix-02 0493029c2f7ed80df6f808fa
codex/uncached/prefix-02 c3a8a7444a873101aca2f8c6
codex/cache-control/prefix-02 c2deb34800efe1c63b380c20
codex/base/prefix-03 cebeb6169396794c78423ff4
codex/instructed/prefix-03 f7ce5f4cb73eb8e57bc73d36
codex/uncached/prefix-03 45d1cfe9cdadd7ccc6acce4a
codex/cache-control/prefix-03 cebeb6169396794c78423ff4
codex/base/prefix-04 aa3a15b496f370a746b61d13
codex/instructed/prefix-04 9c371242a278b5f12f154939
codex/uncached/prefix-04 96375847bf26f80c98b6e5e5
codex/cache-control/prefix-04 aa3a15b496f370a746b61d13
codex/base/prefix-05 92d4eedb4a157620d6c31ce1
codex/instructed/prefix-05 b1bf266159e141c87065371c
codex/uncached/prefix-05 8b0f195552a4235a054cc5ef
codex/cache-control/prefix-05 92d4eedb4a157620d6c31ce1
codex/base/prefix-06 2bb01d87d322f8ce71083376
codex/instructed/prefix-06 8e476d4d981d67456a02225c
codex/uncached/prefix-06 21ccd6f434d93e66a9599d0a
codex/cache-control/prefix-06 2bb01d87d322f8ce71083376
codex/base/prefix-07 1f52097ecdc9976d4d71ae7f
codex/instructed/prefix-07 73e8d9c2df7186c90cfebf0d
codex/uncached/prefix-07 90e90608e3cc44d5d3876525
codex/cache-control/prefix-07 1f52097ecdc9976d4d71ae7f
codex/base/prefix-08 a805fd81171ba677bc1599f1
codex/instructed/prefix-08 a02def3b04ff7a887d37510b
codex/uncached/prefix-08 7fb61d453bcc583b05232473
codex/cache-control/prefix-08 a805fd81171ba677bc1599f1
codex/base/prefix-09 3e5503028b3a82e4c184e01b
codex/instructed/prefix-09 c89f89a43d62ef0b438ca8e9
codex/uncached/prefix-09 27c4c5ebf8e677af1b503243
codex/cache-control/prefix-09 3e5503028b3a82e4c184e01b
codex/base/prefix-10 8211fb478855e9f8858f93ca
codex/instructed/prefix-10 0fb8265087ca011a06419f8e
codex/uncached/prefix-10 adc223d33146b3b12e04acfc
codex/cache-control/prefix-10 8211fb478855e9f8858f93ca
codex/base/prefix-11 8ff1f23a5743e7d04ab8e558
codex/instructed/prefix-11 756546b31f3d5c99b6226440
codex/uncached/prefix-11 05da76b7a4c267540e634f29
codex/cache-control/prefix-11 8ff1f23a5743e7d04ab8e558
codex/base/prefix-12 96d1b306c08742ed9e130099
codex/instructed/prefix-12 9c8532b3978a4536be00d0a7
codex/uncached/prefix-12 9c256bdb8d7c737385f1ab4c
codex/cache-control/prefix-12 96d1b306c08742ed9e130099
codex/base/prefix-13 b28bb4557a0785bd4b26d503
codex/instructed/prefix-13 4dbb49c29cf3728e35de8e1f
codex/uncached/prefix-13 d457ad4668740a18a1bf7ab8
codex/cache-control/prefix-13 b28bb4557a0785bd4b26d503
codex/base/prefix-14 4a16b6130d7e9961b6fa41e0
codex/instructed/prefix-14 d2670f193a61c21987bc3360
codex/uncached/prefix-14 76eb5ec66cea087a9a76e35c
codex/cache-control/prefix-14 4a16b6130d7e9961b6fa41e0
codex/base/empty-history f72738cd1b2a6cb7dc9c8651
codex/instructed/empty-history db9eb51e5041b6e8a441b8be
codex/uncached/empty-history cef513bd88c45ff23fdb61d2
codex/cache-control/empty-history f72738cd1b2a6cb7dc9c8651
codex/base/only-feedback e1cd54b4304d0659f74cc244
codex/instructed/only-feedback 99103e30379276bd875efd0a
codex/uncached/only-feedback bb9c203136b830d91897c8b5
codex/cache-control/only-feedback e1cd54b4304d0659f74cc244
codex/base/unicode 48a08aaca4be325de2c2184a
codex/instructed/unicode 4381e711645fe4b09267d9b3
codex/uncached/unicode fcfa392828b9d0fd4e16b4a6
codex/cache-control/unicode 48a08aaca4be325de2c2184a
";

fn variants(messages: Vec<LlmMessage>) -> Vec<(&'static str, LlmRequest)> {
    let base = request(messages);
    let mut instructed = base.clone();
    instructed.instructions = Some("You are a careful coding agent.".into());
    instructed.tools = Arc::new(vec![corpus_tool()]);
    let mut uncached = base.clone();
    uncached
        .model
        .metadata_mut()
        .request_defaults
        .cache_retention = CacheRetention::None;
    let mut anthropic_cache = base.clone();
    anthropic_cache
        .model
        .metadata_mut()
        .capability
        .cache_control = Some(CacheControlDialect::Anthropic);
    vec![
        ("base", base),
        ("instructed", instructed),
        ("uncached", uncached),
        ("cache-control", anthropic_cache),
    ]
}

async fn lines(
    provider: &mut impl Provider,
    label: &str,
    model: &str,
    corpus: fn(Vec<LlmMessage>) -> Vec<LlmMessage>,
) -> Vec<String> {
    let route = provider.route_identity(model);
    // One cache across the corpus: each prefix reuses the history the
    // previous one lowered, and still sends the recorded bytes.
    let cache = SegmentCache::new();
    let mut lines = Vec::new();
    for (name, messages) in wire_corpus(&route) {
        let messages = corpus(messages);
        for (variant, request) in variants(messages) {
            let lowered = provider.lower(&request, &cache).await;
            lines.push(digest_line(
                &format!("{label}/{variant}/{name}"),
                lowered.as_ref().map_err(|error| error.message.clone()),
            ));
        }
    }
    lines
}

#[tokio::test]
async fn lowering_sends_the_recorded_bytes() {
    let model = "openai/gpt-5.4";
    let mut all = lines(&mut OpenAiProvider::new("key"), "responses", model, |m| m).await;
    all.extend(lines(&mut openrouter_provider(), "chat", model, |m| m).await);
    all.extend(lines(&mut openrouter_provider(), "chat-text", model, text_only).await);
    all.extend(
        lines(
            &mut crate::CodexProvider::new(Arc::new(lash_core::provider::ProviderToken::new(
                "access",
            ))),
            "codex",
            model,
            |m| m,
        )
        .await,
    );
    assert_eq!(all.join("\n"), RECORDED.trim(), "\n{}\n", all.join("\n"));
}

/// The invalidation law: through one cache shared by Responses, Chat
/// Completions and Codex, an edited, renamed or compacted history and
/// another cache configuration each lower to what a cold lowering sends,
/// a refusal included.
#[tokio::test]
async fn a_reused_prefix_sends_the_bytes_a_cold_lowering_sends() {
    let model = "openai/gpt-5.4";
    let cache = SegmentCache::new();
    let retained_cache = SegmentCache::new();
    let budget = lash_protocol_types::allocation::AllocationBudget::unowned();
    let mut providers: Vec<(&str, Box<dyn Provider>)> = vec![
        ("responses", Box::new(OpenAiProvider::new("key"))),
        ("chat", Box::new(openrouter_provider())),
        (
            "codex",
            Box::new(crate::CodexProvider::new(Arc::new(
                lash_core::provider::ProviderToken::new("access"),
            ))),
        ),
    ];
    let route = providers[0].1.route_identity(model);
    for (case, histories) in reuse_cases(&route) {
        for (step, messages) in histories.into_iter().enumerate() {
            for (variant, request) in variants(messages) {
                for (label, provider) in &mut providers {
                    let name = format!("{label} {case} step {step} {variant}");
                    let warm = provider.lower(&request, &cache).await;
                    let cold = provider.lower(&request, &SegmentCache::disabled()).await;
                    let retained =
                        lash_protocol_types::llm::types::RetainedLlmRequest::from_request(
                            request.clone(),
                        );
                    let persistent = provider
                        .lower_retained(&retained, &retained_cache, &budget)
                        .await;
                    assert_eq!(
                        digest_line(
                            &name,
                            persistent.as_ref().map_err(|error| error.message.clone())
                        ),
                        digest_line(&name, cold.as_ref().map_err(|error| error.message.clone())),
                        "retained {name}"
                    );
                    assert_eq!(
                        digest_line(&name, warm.as_ref().map_err(|error| error.message.clone())),
                        digest_line(&name, cold.as_ref().map_err(|error| error.message.clone())),
                    );
                }
            }
        }
    }
    let stats = cache.stats();
    assert!(stats.hits > stats.misses, "{stats:?}");
}
