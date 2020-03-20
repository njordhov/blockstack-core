pub mod parser;
pub mod expression_identifier;
pub mod definition_sorter;
pub mod traits_resolver;

pub mod sugar_expander;
pub mod types;
pub mod errors;
pub mod stack_depth_checker;
use vm::errors::{Error, RuntimeErrorType};
use vm::costs::{cost_functions, CostTracker};

use vm::representations::{SymbolicExpression};
use vm::types::QualifiedContractIdentifier;

pub use self::types::ContractAST;
use self::types::BuildASTPass;
use self::errors::ParseResult;
use self::expression_identifier::ExpressionIdentifier;
use self::sugar_expander::SugarExpander;
use self::definition_sorter::DefinitionSorter;
use self::traits_resolver::TraitsResolver;
use self::stack_depth_checker::StackDepthChecker;

/// Legacy function
#[cfg(test)]
pub fn parse(contract_identifier: &QualifiedContractIdentifier, source_code: &str) -> Result<Vec<SymbolicExpression>, Error> {
    let ast = build_ast(contract_identifier, source_code, &mut ())?;
    Ok(ast.expressions)
}

pub fn build_ast<T: CostTracker>(contract_identifier: &QualifiedContractIdentifier, source_code: &str, cost_track: &mut T) -> ParseResult<ContractAST> {
    runtime_cost!(cost_functions::AST_PARSE, cost_track, source_code.len() as u64)?;
    let pre_expressions = parser::parse(source_code)?;
    let mut contract_ast = ContractAST::new(contract_identifier.clone(), pre_expressions);
    StackDepthChecker::run_pass(&mut contract_ast)?;
    ExpressionIdentifier::run_pass(&mut contract_ast)?;
    DefinitionSorter::run_pass(&mut contract_ast, cost_track)?;
    TraitsResolver::run_pass(&mut contract_ast)?;
    SugarExpander::run_pass(&mut contract_ast)?;
    Ok(contract_ast)
}

#[cfg(test)]
mod tests {
    use vm::costs::LimitedCostTracker;
    use super::*;

    fn dependency_edge_counting_runtime(iters: usize) -> u64 {
        let mut progn = "(define-private (a0) 1)".to_string();
        for i in 1..iters {
            progn.push_str(
                &format!("\n(define-private (a{}) (begin", i));
            for x in 0..i {
                progn.push_str(&format!(" (a{}) ", x));
            }
            progn.push_str("))");
        }

        let mut cost_track = LimitedCostTracker::new_max_limit();
        build_ast(&QualifiedContractIdentifier::transient(), &progn, &mut cost_track).unwrap();

        cost_track.get_total().runtime
    }

    #[test]
    fn test_edge_counting_runtime() {
        let ratio_4_8 = dependency_edge_counting_runtime(8) / dependency_edge_counting_runtime(4);
        let ratio_8_16 = dependency_edge_counting_runtime(16) / dependency_edge_counting_runtime(8);

        // this really is just testing for the non-linearity
        //   in the runtime cost assessment (because the edge count in the dependency graph is going up O(n^2)).
        assert!(ratio_8_16 > ratio_4_8);
    }
}
