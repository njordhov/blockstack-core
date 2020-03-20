use vm::representations::SymbolicExpression;
use vm::types::{Value, AssetIdentifier, PrincipalData, QualifiedContractIdentifier, TypeSignature};
use vm::contexts::{OwnedEnvironment, AssetMap};
use vm::database::{MarfedKV, ClarityDatabase, SqliteConnection, HeadersDB};
use vm::analysis::{AnalysisDatabase};
use vm::errors::{Error as InterpreterError};
use vm::ast::{ContractAST, errors::ParseError};
use vm::analysis::{ContractAnalysis, errors::CheckError, errors::CheckErrors};
use vm::ast;
use vm::analysis;
use vm::costs::{LimitedCostTracker, ExecutionCost};

use chainstate::burn::BlockHeaderHash;
use chainstate::stacks::index::marf::MARF;
use chainstate::stacks::index::TrieHash;

use std::error;
use std::fmt;

///
/// A high-level interface for interacting with the Clarity VM.
///
/// ClarityInstance takes ownership of a MARF + Sqlite store used for
///   it's data operations.
/// The ClarityInstance defines a `begin_block(bhh, bhh, bhh) -> ClarityBlockConnection`
///    function.
/// ClarityBlockConnections are used for executing transactions within the context of 
///    a single block.
/// Only one ClarityBlockConnection may be open at a time (enforced by the borrow checker)
///   and ClarityBlockConnections must be `commit_block`ed or `rollback_block`ed before discarding
///   begining the next connection (enforced by runtime panics).
///
pub struct ClarityInstance {
    datastore: Option<MarfedKV>,
}

///
/// A high-level interface for Clarity VM interactions within a single block.
///
pub struct ClarityBlockConnection<'a> {
    datastore: MarfedKV,
    parent: &'a mut ClarityInstance,
    header_db: &'a dyn HeadersDB,
    cost_track: Option<LimitedCostTracker>
}

#[derive(Debug)]
pub enum Error {
    Analysis(CheckError),
    Parse(ParseError),
    Interpreter(InterpreterError),
    BadTransaction(String),
    CostError(ExecutionCost, ExecutionCost),
}

impl From<CheckError> for Error {
    fn from(e: CheckError) -> Self {
        Error::Analysis(e)
    }
}

impl From<InterpreterError> for Error {
    fn from(e: InterpreterError) -> Self {
        match &e {
            InterpreterError::Unchecked(CheckErrors::CostBalanceExceeded(a, b)) => Error::CostError(a.clone(), b.clone()),
            InterpreterError::Unchecked(CheckErrors::CostOverflow) => Error::CostError(ExecutionCost::max_value(), ExecutionCost::max_value()),
            _ => Error::Interpreter(e)
        }
    }
}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Error::Parse(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::CostError(ref a, ref b) => write!(f, "Cost Error: {} cost exceeded budget of {} cost", a, b),
            Error::Analysis(ref e) => fmt::Display::fmt(e, f),
            Error::Parse(ref e) => fmt::Display::fmt(e, f),
            Error::Interpreter(ref e) => fmt::Display::fmt(e, f),
            Error::BadTransaction(ref s) => fmt::Display::fmt(s, f)
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::CostError(ref _a, ref _b) => None,
            Error::Analysis(ref e) => Some(e),
            Error::Parse(ref e) => Some(e),
            Error::Interpreter(ref e) => Some(e),
            Error::BadTransaction(ref _s) => None
        }
    }
}

impl ClarityInstance {
    pub fn new(datastore: MarfedKV) -> ClarityInstance {
        ClarityInstance { datastore: Some(datastore) }
    }

    pub fn begin_block<'a> (&'a mut self, current: &BlockHeaderHash, next: &BlockHeaderHash,
                            header_db: &'a dyn HeadersDB) -> ClarityBlockConnection<'a> {
        let mut datastore = self.datastore.take()
            // this is a panicking failure, because there should be _no instance_ in which a ClarityBlockConnection
            //   doesn't restore it's parent's datastore
            .expect("FAIL: use of begin_block while prior block neither committed nor rolled back.");

        datastore.begin(current, next);

        ClarityBlockConnection {
            datastore,
            header_db,
            parent: self,
            cost_track: Some(LimitedCostTracker::new_max_limit())
        }
    }

    pub fn begin_block_with_limit<'a> (&'a mut self, current: &BlockHeaderHash, next: &BlockHeaderHash,
                                       header_db: &'a dyn HeadersDB, limit: ExecutionCost) -> ClarityBlockConnection<'a> {
        let mut datastore = self.datastore.take()
            // this is a panicking failure, because there should be _no instance_ in which a ClarityBlockConnection
            //   doesn't restore it's parent's datastore
            .expect("FAIL: use of begin_block while prior block neither committed nor rolled back.");

        datastore.begin(current, next);

        ClarityBlockConnection {
            datastore,
            header_db,
            parent: self,
            cost_track: Some(LimitedCostTracker::new(limit))
        }
    }

    #[cfg(test)]
    pub fn eval_read_only(&mut self, at_block: &BlockHeaderHash, header_db: &dyn HeadersDB,
                          contract: &QualifiedContractIdentifier, program: &str) -> Result<Value, Error> {
        self.datastore.as_mut().unwrap()
            .set_chain_tip(at_block);
        let clarity_db = self.datastore.as_mut().unwrap()
            .as_clarity_db(header_db);
        let mut env = OwnedEnvironment::new(clarity_db);
        env.eval_read_only(contract, program)
            .map(|(x, _)| x)
            .map_err(Error::from)
    }

    pub fn destroy(mut self) -> MarfedKV {
        let datastore = self.datastore.take()
            .expect("FAIL: attempt to recover database connection from clarity instance which is still open");

        datastore
    }
}

impl <'a> ClarityBlockConnection <'a> {
    /// Rolls back all changes in the current block by
    /// (1) dropping all writes from the current MARF tip,
    /// (2) rolling back side-storage
    pub fn rollback_block(mut self) {
        // this is a "lower-level" rollback than the roll backs performed in
        //   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
        debug!("Rollback Clarity datastore");
        self.datastore.rollback();

        self.parent.datastore.replace(self.datastore);
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    #[cfg(test)]
    pub fn commit_block(mut self) -> LimitedCostTracker {
        debug!("Commit Clarity datastore");
        self.datastore.test_commit();

        self.parent.datastore.replace(self.datastore);

        self.cost_track.unwrap()
    }
    
    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.  Commits to a different 
    /// block hash than the one opened (i.e. since the caller
    /// may not have known the "real" block hash at the 
    /// time of opening).
    pub fn commit_to_block(mut self, final_bhh: &BlockHeaderHash) -> LimitedCostTracker {
        debug!("Commit Clarity datastore to {}", final_bhh);
        self.datastore.commit_to(final_bhh);

        self.parent.datastore.replace(self.datastore);

        self.cost_track.unwrap()
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    ///    before this saves, it updates the metadata headers in
    ///    the sidestore so that they don't get stepped on after
    ///    a miner re-executes a constructed block.
    pub fn commit_block_will_move(mut self, will_move: &str) -> LimitedCostTracker {
        debug!("Commit Clarity datastore to {}", will_move);
        self.datastore.commit_for_move(will_move);

        self.parent.datastore.replace(self.datastore);

        self.cost_track.unwrap()
    }

    /// Get the MARF root hash
    pub fn get_root_hash(&mut self) -> TrieHash {
        self.datastore.get_root_hash()
    }

    /// Get the inner MARF
    pub fn get_marf(&mut self) -> &mut MARF {
        self.datastore.get_marf()
    }

    /// Do something to the underlying DB that involves writing.
    pub fn with_clarity_db<F, R>(&mut self, to_do: F) -> Result<R, Error>
    where F: FnOnce(&mut ClarityDatabase) -> Result<R, Error> {
        let mut db = ClarityDatabase::new(&mut self.datastore, &self.header_db);
        db.begin();
        let result = to_do(&mut db);
        match result {
            Ok(r) => {
                db.commit();
                Ok(r)
            },
            Err(e) => {
                db.roll_back();
                Err(e)
            }
        }
    }
    
    /// Do something to the underlying DB that involves only reading.
    pub fn with_clarity_db_readonly<F, R>(&mut self, to_do: F) -> Result<R, Error>
    where F: FnOnce(&mut ClarityDatabase) -> Result<R, Error> {
        let mut db = ClarityDatabase::new(&mut self.datastore, &self.header_db);
        db.begin();
        let result = to_do(&mut db);
        db.roll_back();
        result
    }

    /// Analyze a provided smart contract, but do not write the analysis to the AnalysisDatabase
    pub fn analyze_smart_contract(&mut self, identifier: &QualifiedContractIdentifier, contract_content: &str)
                                  -> Result<(ContractAST, ContractAnalysis), Error> {
        let mut db = AnalysisDatabase::new(&mut self.datastore);

        let mut contract_ast = ast::build_ast(identifier, contract_content,
                                              self.cost_track.as_mut()
                                              .expect("Failed to get ownership of cost tracker"))?;

        let cost_track = self.cost_track.take()
            .expect("Failed to get ownership of cost tracker in ClarityBlockConnection");

        let mut contract_analysis = analysis::run_analysis(identifier, &mut contract_ast.expressions,
                                                       &mut db, false, cost_track)?;

        let cost_track = contract_analysis.take_contract_cost_tracker();
        self.cost_track.replace(cost_track);

        Ok((contract_ast, contract_analysis))
    }

    fn with_abort_callback<F, A, R>(&mut self, to_do: F, abort_call_back: A) -> Result<(R, AssetMap), Error>
    where A: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool,
          F: FnOnce(&mut OwnedEnvironment) -> Result<(R, AssetMap), Error> {
        let mut db = ClarityDatabase::new(&mut self.datastore, &self.header_db);
        // wrap the whole contract-call in a claritydb transaction,
        //   so we can abort on call_back's boolean retun
        db.begin();
        let cost_track = self.cost_track.take()
            .expect("Failed to get ownership of cost tracker in ClarityBlockConnection");
        let mut vm_env = OwnedEnvironment::new_cost_limited(db, cost_track);
        let result = to_do(&mut vm_env);
        let (mut db, cost_track) = vm_env.destruct()
            .expect("Failed to recover database reference after executing transaction");
        self.cost_track.replace(cost_track);

        match result {
            Ok((value, asset_map)) => {
                if abort_call_back(&asset_map, &mut db) {
                    db.roll_back();
                } else {
                    db.commit();
                }
                Ok((value, asset_map))
            },
            Err(e) => {
                db.roll_back();
                Err(e)
            }
        }
    }
    

    /// Save a contract analysis output to the AnalysisDatabase
    /// An error here would indicate that something has gone terribly wrong in the processing of a contract insert.
    ///   the caller should likely abort the whole block or panic
    pub fn save_analysis(&mut self, identifier: &QualifiedContractIdentifier, contract_analysis: &ContractAnalysis) -> Result<(), CheckError> {
        let mut db = AnalysisDatabase::new(&mut self.datastore);
        db.begin();
        let result = db.insert_contract(identifier, contract_analysis);
        match result {
            Ok(_) => {
                db.commit();
                Ok(())
            },
            Err(e) => {
                db.roll_back();
                Err(e)
            }
        }
    }

    /// Execute a contract call in the current block.
    ///  If an error occurs while processing the transaction, it's modifications will be rolled back.
    /// abort_call_back is called with an AssetMap and a ClarityDatabase reference,
    ///   if abort_call_back returns false, all modifications from this transaction will be rolled back.
    ///      otherwise, they will be committed (though they may later be rolled back if the block itself is rolled back).
    pub fn run_contract_call <F> (&mut self, sender: &PrincipalData, contract: &QualifiedContractIdentifier, public_function: &str,
                                  args: &[Value], abort_call_back: F) -> Result<(Value, AssetMap), Error>
    where F: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool {
        let expr_args: Vec<_> = args.iter().map(|x| SymbolicExpression::atom_value(x.clone())).collect();

        self.with_abort_callback(
            |vm_env| { vm_env.execute_transaction(Value::Principal(sender.clone()), contract.clone(), public_function, &expr_args)
                       .map_err(Error::from) },
            abort_call_back)
    }

    /// Initialize a contract in the current block.
    ///  If an error occurs while processing the initialization, it's modifications will be rolled back.
    /// abort_call_back is called with an AssetMap and a ClarityDatabase reference,
    ///   if abort_call_back returns false, all modifications from this transaction will be rolled back.
    ///      otherwise, they will be committed (though they may later be rolled back if the block itself is rolled back).
    pub fn initialize_smart_contract <F> (&mut self, identifier: &QualifiedContractIdentifier, contract_ast: &ContractAST,
                                          contract_str: &str, abort_call_back: F) -> Result<AssetMap, Error>
    where F: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool {
        let (_, asset_map) = self.with_abort_callback(
            |vm_env| { vm_env.initialize_contract_from_ast(identifier.clone(), contract_ast, contract_str)
                       .map_err(Error::from) },
            abort_call_back)?;
        Ok(asset_map)
    }

    /// Evaluate a raw Clarity snippit
    #[cfg(test)]
    pub fn clarity_eval_raw(&mut self, code: &str) -> Result<Value, Error> {
        let (result, _) = self.with_abort_callback(
            |vm_env| { vm_env.eval_raw(code).map_err(Error::from) },
            |_, _| { false })?;
        Ok(result)
    }

    #[cfg(test)]
    pub fn eval_read_only(&mut self, contract: &QualifiedContractIdentifier, code: &str) -> Result<Value, Error> {
        let (result, _) = self.with_abort_callback(
            |vm_env| { vm_env.eval_read_only(contract, code).map_err(Error::from) },
            |_, _| { false })?;
        Ok(result)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use vm::analysis::errors::CheckErrors;
    use vm::types::{Value, StandardPrincipalData};
    use vm::database::{NULL_HEADER_DB, ClarityBackingStore, MarfedKV};
    use chainstate::stacks::index::storage::{TrieFileStorage};
    use rusqlite::NO_PARAMS;

    #[test]
    pub fn simple_test() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf);

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        {
            let mut conn = clarity_instance.begin_block(&TrieFileStorage::block_sentinel(),
                                                        &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                        &NULL_HEADER_DB);
            
            let contract = "(define-public (foo (x int)) (ok (+ x x)))";
            
            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&contract_identifier, &contract).unwrap();
            conn.initialize_smart_contract(
                &contract_identifier, &ct_ast, &contract, |_,_| false).unwrap();
            conn.save_analysis(&contract_identifier, &ct_analysis).unwrap();
            
            assert_eq!(
                conn.run_contract_call(&StandardPrincipalData::transient().into(), &contract_identifier, "foo", &[Value::Int(1)],
                                       |_, _| false).unwrap().0,
                Value::okay(Value::Int(2)).unwrap());
            
            conn.commit_block();
        }
        let mut marf = clarity_instance.destroy();
        assert!(marf.get_contract_hash(&contract_identifier).is_ok());
    }

    #[test]
    pub fn test_block_roll_back() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        {
            let mut conn = clarity_instance.begin_block(&TrieFileStorage::block_sentinel(),
                                                        &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                        &NULL_HEADER_DB);

            let contract = "(define-public (foo (x int)) (ok (+ x x)))";

            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&contract_identifier, &contract).unwrap();
            conn.initialize_smart_contract(
                &contract_identifier, &ct_ast, &contract, |_,_| false).unwrap();
            conn.save_analysis(&contract_identifier, &ct_analysis).unwrap();
            
            conn.rollback_block();
        }

        let mut marf = clarity_instance.destroy();
        // should not be in the marf.
        assert_eq!(marf.get_contract_hash(&contract_identifier).unwrap_err(),
                   CheckErrors::NoSuchContract(contract_identifier.to_string()).into());
        let sql = marf.get_side_store();
        // sqlite should not have any entries
        assert_eq!(0,
                   sql.mut_conn()
                   .query_row::<u32,_,_>("SELECT COUNT(value) FROM data_table", NO_PARAMS, |row| row.get(0)).unwrap());
    }

    #[test]
    pub fn test_tx_roll_backs() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        {
            let mut conn = clarity_instance.begin_block(&TrieFileStorage::block_sentinel(),
                                                        &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                        &NULL_HEADER_DB);

            let contract = "
            (define-data-var bar int 0)
            (define-public (get-bar) (ok (var-get bar)))
            (define-public (set-bar (x int) (y int))
              (begin (var-set bar (/ x y)) (ok (var-get bar))))";

            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&contract_identifier, &contract).unwrap();
            conn.initialize_smart_contract(
                &contract_identifier, &ct_ast, &contract, |_,_| false).unwrap();
            conn.save_analysis(&contract_identifier, &ct_analysis).unwrap();

            assert_eq!(
                conn.run_contract_call(&sender, &contract_identifier, "get-bar", &[],
                                       |_, _| false).unwrap().0,
                Value::okay(Value::Int(0)).unwrap());

            assert_eq!(
                conn.run_contract_call(&sender, &contract_identifier, "set-bar", &[Value::Int(1), Value::Int(1)],
                                       |_, _| false).unwrap().0,
                Value::okay(Value::Int(1)).unwrap());

            assert_eq!(
                conn.run_contract_call(&sender, &contract_identifier, "set-bar", &[Value::Int(10), Value::Int(1)],
                                       |_, _| true).unwrap().0,
                Value::okay(Value::Int(10)).unwrap());

            // prior transaction should have rolled back due to abort call back!
            assert_eq!(
                conn.run_contract_call(&sender, &contract_identifier, "get-bar", &[],
                                       |_, _| false).unwrap().0,
                Value::okay(Value::Int(1)).unwrap());

            assert!(
                format!("{:?}",
                        conn.run_contract_call(&sender, &contract_identifier, "set-bar", &[Value::Int(10), Value::Int(0)],
                                               |_, _| true).unwrap_err())
                    .contains("DivisionByZero"));

            // prior transaction should have rolled back due to runtime error
            assert_eq!(
                conn.run_contract_call(&StandardPrincipalData::transient().into(), &contract_identifier, "get-bar", &[],
                                       |_, _| false).unwrap().0,
                Value::okay(Value::Int(1)).unwrap());

            
            conn.commit_block();
        }
    }

    #[test]
    pub fn test_block_limit() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        {
            let mut conn = clarity_instance.begin_block(&TrieFileStorage::block_sentinel(),
                                                        &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                        &NULL_HEADER_DB);

            let contract = "
            (define-public (do-expand)
              (let ((list1 (list 1 2 3 4 5 6 7 8 9 10)))
                (let ((list2 (concat list1 list1)))
                  (let ((list3 (concat list2 list2)))
                    (let ((list4 (concat list3 list3)))
                      (ok (concat list4 list4)))))))
            ";

            let (ct_ast, ct_analysis) = conn.analyze_smart_contract(&contract_identifier, &contract).unwrap();
            conn.initialize_smart_contract(
                &contract_identifier, &ct_ast, &contract, |_,_| false).unwrap();
            conn.save_analysis(&contract_identifier, &ct_analysis).unwrap();

            conn.commit_block();
        }

        {
            let mut conn = clarity_instance.begin_block_with_limit(&BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                                                                   &BlockHeaderHash::from_bytes(&[1 as u8; 32]).unwrap(),
                                                                   &NULL_HEADER_DB,
                                                                   ExecutionCost {
                                                                       write_length: u64::max_value(),
                                                                       write_count: u64::max_value(),
                                                                       read_count: u64::max_value(),
                                                                       read_length: u64::max_value(),
                                                                       runtime: 100
                                                                   });
            assert!(
                match conn.run_contract_call(&sender, &contract_identifier, "do-expand", &[],
                                       |_, _| false).unwrap_err() {
                    Error::CostError(total, limit) => {
                        eprintln!("{}, {}", total, limit);
                        (limit.runtime == 100 && total.runtime > 100)
                    },
                    x => {
                        eprintln!("{}", x);
                        false
                    }
                }
            );

            conn.commit_block();
        }
    }
}
