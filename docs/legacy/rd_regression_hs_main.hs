module Main where

-- RD 回归测试：真实引擎实验（2026-08-16）结果的固化。
-- 实验：leaky(68ms, est_loss=1/3) vs faithful(418ms, est_loss=0)
-- 断言：λ* 翻转点 ≈ 129649；GCF v4.1 九列落盘 + 老七列兼容解析。

import qualified Data.Text as T
import qualified Data.Map.Strict as M
import qualified Data.Set
import Data.List (sort)
import System.Directory (createDirectoryIfMissing, doesFileExist, doesDirectoryExist, removeDirectoryRecursive)
import System.Environment (setEnv)
import Control.Monad (unless)
import Exec
import qualified Data.Text.IO as TIO
import AST

tmpHome :: IO ()
tmpHome = do
  let h = "/tmp/rd-test-home"
  fp <- doesFileExist h
  dp <- doesDirectoryExist h
  if fp || dp then removeDirectoryRecursive h else return ()
  createDirectoryIfMissing True h

main :: IO ()
main = do
  putStrLn "== RD regression tests (real-engine experiment 2026-08-16) =="
  putStrLn ""
  putStrLn "-- T1: resultFieldKeys handles both encodings"
  let ssEnc = T.pack "\167\167FIELDS\167\167title=X\167\167authors=Y\167\167RAW\167\167full"
      dslEnc = T.unlines (map T.pack ["##DSL_RESULT","title=X","authors=Y","##DSL_END"])
  let ks1 = sort (resultFieldKeys ssEnc)
      ks2 = sort (resultFieldKeys dslEnc)
  check "T1a §§ encoding keys" (ks1 == map T.pack ["authors","title"])
  check "T1b DSL block keys" (ks2 == map T.pack ["authors","title"])

  putStrLn "-- T2: estLossFieldCoverage v1"
  let up = map T.pack ["title","authors","refs"]
      leakyOut = T.pack "\167\167FIELDS\167\167title=X\167\167authors=Y\167\167RAW\167\167..."
      faithfulOut = T.pack "\167\167FIELDS\167\167title=X\167\167authors=Y\167\167refs=12\167\167RAW\167\167..."
  case estLossFieldCoverage up leakyOut of
    Just l -> check "T2a leaky est_loss = 1/3" (abs (l - 1/3) < 1e-9)
    Nothing -> fail_ "T2a leaky est_loss returned Nothing"
  case estLossFieldCoverage up faithfulOut of
    Just l -> check "T2b faithful est_loss = 0" (abs l < 1e-12)
    Nothing -> fail_ "T2b faithful est_loss returned Nothing"

  putStrLn "-- T3: ranking flip at lambda*"
  let rrLeaky = RecentRuns 20 0 0 68 123.0 (1/3)
      rrFaith = RecentRuns 20 0 0 418 124.0 0.0
      recentMap = M.fromList [(T.pack "leaky", rrLeaky), (T.pack "faithful", rrFaith)]
      impls = [ mkImpl (T.pack "leaky"), mkImpl (T.pack "faithful") ]
      lam0 = rankImpls 0.0 recentMap impls
      lamBig = rankImpls 200000.0 recentMap impls
      firstImpl = implName . fst . head
  check "T3a lambda=0 picks leaky (fastest)" (firstImpl lam0 == T.pack "leaky")
  check "T3b lambda=200000 picks faithful (loss-aware)" (firstImpl lamBig == T.pack "faithful")
  let lamStar = (418 - 68) / (rdSurcharge rrLeaky - rdSurcharge rrFaith)
  check "T3c lambda* = 129150 (= 350*369 exactly)" (abs (lamStar - 129150.0) < 1.0)

  putStrLn "-- T4: GCF v4.1 write + parse roundtrip (isolated HOME)"
  setEnv "HOME" "/tmp/rd-test-home"   -- must be set BEFORE tmpHome/appendRunRD
  tmpHome
  let impl = mkImpl (T.pack "leaky")
  appendRunRD (T.pack "compress") impl 'A' Ok 68 Nothing Nothing leakyOut up
  let gcf = "/tmp/rd-test-home/.local/share/ductile/records/compress/runs.gcf"
  exists <- doesFileExist gcf
  check "T4a file created" exists
  content <- T.unpack <$> TIO.readFile gcf
  check "T4b 9-col format (rateTokens + estLoss)" (length (filter (== '|') (head (drop 2 (lines content)))) == 8)
  rr <- loadRecentRuns (T.pack "compress") "/tmp/rd-test-home"
  case M.lookup (T.pack "leaky") rr of
    Just r -> do
      -- rate = 本输出字符数/4: §§FIELDS§§title=X§§authors=Y§§RAW§§... = 38 chars → 9.5
      check "T4c rrSumTokens = len(output)/4" (abs (rrSumTokens r - 9.5) < 1e-9)
      check "T4d rrSumLoss roundtrip (1/3)" (abs (rrSumLoss r - 0.3333333333333333) < 1e-9)
      check "T4e rrAvgMs roundtrip" (abs (rrAvgMs r - 68) < 1e-9)
    Nothing -> fail_ "T4c loadRecentRuns lost the impl"

  putStrLn "-- T5: old 7-col rows still parse (backward compat)"
  let oldRow = "2026-08-08T02:44:28|llm_decompose|Ok|0|A|-|-"
  writeFile "/tmp/rd-test-home/.local/share/ductile/records/compress/runs.gcf" $
    "GCF profile=generic\n## runs\n" ++ oldRow ++ "\n"
  rrOld <- loadRecentRuns (T.pack "compress") "/tmp/rd-test-home"
  case M.lookup (T.pack "llm_decompose") rrOld of
    Just r -> check "T5b old row RD cols = 0" (rrSumTokens r == 0 && rrSumLoss r == 0)
    Nothing -> fail_ "T5b lookup failed"

  putStrLn "-- T6: no double-billing — Fail without v1 evidence bills 0 to RD domain"
  -- Fail 且无结构化证据（无上游字段/无DSL_RESULT）→ estLoss = 0，失败只由 fail-rate 惩罚计费
  writeFile "/tmp/rd-test-home/.local/share/ductile/records/compress/runs.gcf" $
    "GCF profile=generic\n## runs\n"
  let noUp = [] :: [T.Text]
  appendRunRD (T.pack "compress") (mkImpl (T.pack "crasher")) 'A' Fail 50 Nothing (Just (T.pack "abc12345")) (T.pack "raw error output") noUp
  rrF <- loadRecentRuns (T.pack "compress") "/tmp/rd-test-home"
  case M.lookup (T.pack "crasher") rrF of
    Just r -> do
      check "T6a Fail no-evidence estLoss = 0" (rrSumLoss r == 0)
      check "T6b Fail still counted in fail domain" (rrFails r == 1)
    Nothing -> fail_ "T6a lookup failed"
  -- 对照：Fail 但有 v1 证据（上游有字段、输出结构化缺字段）→ RD 域正常计费
  appendRunRD (T.pack "compress") (mkImpl (T.pack "structfail")) 'A' Fail 50 Nothing (Just (T.pack "abc12345")) leakyOut up
  rrG <- loadRecentRuns (T.pack "compress") "/tmp/rd-test-home"
  case M.lookup (T.pack "structfail") rrG of
    Just r -> check "T6c Fail with v1 evidence still bills RD" (abs (rrSumLoss r - 0.3333333333333333) < 1e-9)
    Nothing -> fail_ "T6c lookup failed"

  _ <- tryRemoveDir "/tmp/rd-test-home"
  putStrLn ""
  putStrLn "ALL PASS"
  where
    tryRemoveDir p = do
      e <- doesFileExist p
      if e then removeDirectoryRecursive p >> return () else return ()
    check name True = putStrLn ("  PASS  " ++ name)
    check name False = putStrLn ("  FAIL  " ++ name) >> error ("assertion failed: " ++ name)
    fail_ name = putStrLn ("  FAIL  " ++ name) >> error name
    mkImpl n = Impl
      { implName = n
      , implDescription = T.empty
      , implTags = Data.Set.empty
      , implSteps = []
      , implEnabled = True
      , implWhen = Nothing
      , implRefs = []
      , implBodyText = T.empty
      , implRetry = 0
      , implEnsure = []
      }
