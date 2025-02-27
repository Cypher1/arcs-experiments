mod parser;
mod ast;
mod ir_gen;
mod graph;
mod graph_builder;
mod graph_to_module;

use inkwell::targets::{InitializationConfig, Target, TargetMachine, TargetTriple, RelocMode, CodeModel, FileType};
use inkwell::OptimizationLevel;
use inkwell::context::Context;

use nom::Err;

use std::env;
use std::path::Path;
use std::fs::File;
use std::io::{prelude::*, stdout, stderr};

use std::collections::HashMap;

use std::process::Command;

use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug)]
enum SkunkError {
  FileNotFound(String),
  FileUnreadable,
  ParseFailed,
  GraphBuilderError(graph_builder::GraphBuilderError),
  GraphToModuleError(graph_to_module::GraphToModuleError),
  CodegenError(ir_gen::codegen_state::CodegenError),
}

impl From<graph_builder::GraphBuilderError> for SkunkError {
  fn from(item: graph_builder::GraphBuilderError) -> SkunkError {
    SkunkError::GraphBuilderError(item)
  }
}

impl <'a> From<graph_to_module::GraphToModuleError> for SkunkError {
  fn from(item: graph_to_module::GraphToModuleError) -> SkunkError {
    SkunkError::GraphToModuleError(item)
  }
}

impl <'a> From<ir_gen::codegen_state::CodegenError> for SkunkError {
  fn from(item: ir_gen::codegen_state::CodegenError) -> SkunkError {
    SkunkError::CodegenError(item)
  }
}

struct FileData {
  buffer: String,
  ast: Vec<ast::TopLevel>,
  main_module: Option<Rc<ast::Module>>,
}


struct MainData {
  file_info: RefCell<HashMap<String, Rc<FileData>>>,
}

impl MainData {
  fn new() -> Self {
    Self { file_info: RefCell::new(HashMap::new()) }
  }
  fn load_file(&self, location: &str) -> Result<(), SkunkError> {
    let mut file_data = FileData::new();
    file_data.prepare(self, location)?;
    {
      self.file_info.borrow_mut().insert(location.to_string(), Rc::new(file_data));
    }
    Ok(())
  }
  fn main_module_for_file(&self, location: &str) -> Option<Rc<ast::Module>> {
    let existing_data = {
      let file_info = self.file_info.borrow();
      file_info.get(location).map(|r| (*r).clone())
    };
    match existing_data {
      None => {
        self.load_file(location).ok()?;
        self.file_info.borrow().get(location).and_then(|file_info| (file_info.main_module.clone()))    
      }
      Some(info) => info.main_module.clone()
    }
  }
}

impl FileData {
  fn new() -> Self {
    Self { ast: Vec::new(), main_module: None, buffer: String::new() }
  }

  fn prepare(&mut self, main_data: &MainData, location: &str) -> Result<(), SkunkError> {
    dbg!(location);
    let slash = location.rfind("/");
    let prefix = match slash {
      None => "./",
      Some(pos) => &location[0..pos+1]
    };
    dbg!(prefix);
    let mut f = File::open(location).or(Err(SkunkError::FileNotFound(location.to_string())))?;
    f.read_to_string(&mut self.buffer).or(Err(SkunkError::FileUnreadable))?;

    let (remainder, mut ast) = match parser::parse(&self.buffer) {
      Ok(result) => result,
      Err(Err::Failure(e) | Err::Error(e)) => { 
        println!("{}", e);
        return Err(SkunkError::ParseFailed);
      }
      Err(Err::Incomplete(_n)) => panic!("Should not be possible")
    };
    
    if remainder.fragment().len() > 0 {
      println!("Left over: {}", remainder);
    }
  
    let dependencies = ast::uses(&ast);
    let mut processed_modules = Vec::new();
    for dependency in dependencies {
      // TODO: Absolute paths, imports from other places, etc. etc.
      let file_name = format!("{}{}.skunk", prefix, dependency.name);
      let module = main_data.main_module_for_file(&file_name).ok_or(SkunkError::FileNotFound(file_name.clone()))?;
      processed_modules.push(module);
    }

    let newtypes = ast::newtypes(&ast).iter().map(|a| (*a).clone()).collect();

    { 
      let mut modules = ast::modules_mut(&mut ast);
      for i in 0..modules.len() {
        modules[i].resolve_types(&newtypes);
        if modules[i].graph.len() > 0 {
          let mut graph = graph_builder::make_graph(modules[i].graph.iter().collect());
          let processed_refs = processed_modules.iter().map(|r| r.as_ref()).collect();
          graph_builder::resolve_graph(modules[i], &processed_refs, &mut graph)?;
          graph_to_module::graph_to_module(modules[i], graph, processed_refs)?;
          println!("{}", modules[i].minidump());
        }
        processed_modules.push(Rc::new(modules[i].clone()))
      }
    }
    self.ast = ast;
    let ast_graphs = ast::graphs(&self.ast);
    let processed_refs = processed_modules.iter().map(|r| r.as_ref()).collect();

    // TODO: Instead of duplicating graph processing logic, push the main module onto the end of the mutable modules list and
    // deal with it in the same pass as the rest.
    if ast_graphs.len() > 0 {
      let mut graph = graph_builder::make_graph(ast::graphs(&self.ast));
      let mut main = ast::Module::create("Main", Vec::new(), Vec::new(), Vec::new(), ast::Examples { examples: Vec::new() }, Vec::new(), Vec::new());

      graph_builder::resolve_graph(&main, &processed_refs, &mut graph)?;

      graph_to_module::graph_to_module(&mut main, graph, processed_refs)?;
      self.main_module = Some(Rc::new(main));
    } else if processed_refs.len() == 1 {
      // TODO: This isn't really correct - there needs to be some way
      // of determining which module is "main".
      self.main_module = Some(Rc::new(processed_refs[0].clone()));
    } else {
      let slash_pos = location.rfind('/');
      // TODO: This maybe assumes ASCII (byte boundary == character boundary)
      let module_name = match slash_pos {
        None => location,
        Some(pos) => location.split_at(pos + 1).1,
      };
      let module_name = module_name.strip_suffix(".skunk").unwrap();
      dbg!(&module_name);
      self.main_module = processed_refs.iter().find(|module| module.name == module_name).map(|module| Rc::new((*module).clone()))
    }
    Ok(())
  }
}

fn main() {
  let args: Vec<String> = env::args().collect();

  if args.len() > 2 && args[1] == "examples" {
    let file = &args[2];
    let mut main_data = MainData::new();
    build_test_examples(&mut main_data, file).unwrap();
    return;
  }

  let (target_triple, target_machine) = target_triple_and_machine();
  let context = Context::create();

  let mut main_data = MainData::new();
  main_data.load_file("test.skunk").unwrap();
  let main = main_data.main_module_for_file("test.skunk").unwrap();

  let mut target_info = ir_gen::codegen_state::TargetInfo { target_machine: &target_machine, target_triple: &target_triple };
  let cg_modules = ir_gen::codegen(&context, &mut target_info, &main).unwrap();

  for module in cg_modules {
    let name = module.get_name().to_str().unwrap();
    println!("Outputting object file for {}", name);
    // module.print_to_stderr();
    let object_name = name.to_string() + ".o";
    let path = Path::new(&object_name);
    target_machine.write_to_file(&module, FileType::Object, path).unwrap();
  }

}



// TODO: have this return a TargetInfo instead?
fn target_triple_and_machine() -> (TargetTriple, TargetMachine) {
  Target::initialize_all(&InitializationConfig::default());

  let target_triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&target_triple).unwrap();
  let target_machine = target.create_target_machine(&target_triple, "generic", "", OptimizationLevel::Default, RelocMode::Default, CodeModel::Default).unwrap();
  (target_triple, target_machine)
}

fn build_test_examples(main_data: &mut MainData, location: &str) -> Result<(), SkunkError> {
  main_data.load_file(location)?;
  let main_module = main_data.main_module_for_file(location).unwrap();

  let (target_triple, target_machine) = target_triple_and_machine();
  let mut target_info = ir_gen::codegen_state::TargetInfo { target_machine: &target_machine, target_triple: &target_triple };

  let context = Context::create();

  // we need object code for all of these
  let cg_modules = ir_gen::codegen(&context, &mut target_info, &main_module)?;

  let mut objects: Vec<String> = Vec::new();
  for module in &cg_modules {
    // module.print_to_stderr();
    let name = module.get_name().to_str().unwrap();
    let object_name = name.to_string() + ".o";
    let path = Path::new(&object_name);
    target_machine.write_to_file(&module, FileType::Object, path).unwrap();
    objects.push(object_name);
  }

  let main_module = ir_gen::main_for_examples(&context, &target_machine, &target_triple, &cg_modules)?;
  let object_name = "main.o";
  let path = Path::new(&object_name);
  target_machine.write_to_file(&main_module, FileType::Object, path).unwrap();
  objects.push(object_name.to_string());

  let mut command = Command::new("clang");
  let mut cmd = command.arg("-o").arg(&(location.to_string() + "_examples"));
  dbg!(&objects);
  for object in objects {
    cmd = cmd.arg(object);
  }

  let output = cmd.arg("-lc").output().expect("failed to run clang");
  stdout().write_all(&output.stdout).unwrap();
  stderr().write_all(&output.stderr).unwrap();

  Ok(())
}