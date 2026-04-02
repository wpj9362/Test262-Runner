/*
  有关Harness的几点说明
  1. 这是可以用来测试自研引擎的Test262通过率的Harness。可实现输出通过率、报错日志等功能。
  2. 能支持Node.js和boa两种引擎的测试，测自研引擎应该也可以。
  3. boa的通过率达到91.94%，距离官方公布的94%存在一定差距，原因可能是因为官方采用了Ignore List忽略部分测试用例等原因造成的，Harness应该问题不大（有问题再说）。
  4. 这是在Windows上开发的，在Linux环境运行需要改几个路径（test_dir,harness_dir、引擎的路径)的命名方式。
*/

use serde::Deserialize;
use std::fs;
use walkdir::WalkDir;
use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::{atomic::{AtomicUsize, Ordering}, Mutex};
use rayon::prelude::*; 

//若出现负面测试用例，这个函数提供了判别标准
#[derive(Debug,Deserialize)]
struct NegativeRule {
    phase: Option<String>,
    #[serde(rename = "type")]
    error_type: Option<String>,
}

//存储Test里面的一些具体信息，还包括一些特殊规则，用于告知如何执行这些测试用例
#[derive(Debug,Deserialize)]
struct TestRule{
    description:String,
    flags: Option<Vec<String>>,     
    features: Option<Vec<String>>,  
    includes: Option<Vec<String>>,
    negative: Option<NegativeRule>,
}

//提取YAML的内容
fn extract_frontmatter(content: &str) -> Option<&str> {
    let start_tag = "/*---";
    let end_tag = "---*/";
    let start_idx = content.find(start_tag)?;
    let end_idx = content[start_idx..].find(end_tag)?; 
    Some(&content[start_idx + 5 .. start_idx + end_idx])
}
//提取执行的jsCode内容
fn extract_js_code(content: &str) -> &str {
    let end_tag = "---*/";
    if let Some(idx) = content.find(end_tag) {
        &content[idx + end_tag.len()..]
    } else {
        content
    }
}

fn main() {
    //test 和 harness（需要将断言assert.js和sta.js异步库导入内存）的目录，需要根据实际情况修改。
    let test_dir = "D:\\test262\\test262\\test"; 
    let harness_dir = "D:\\test262\\test262\\harness";
    
    // 输入目标引擎的名字
    let engine_cmd_name ="boa";// "D:\\test262\\js-engine\\target\\release\\js-engine.exe"; 
    
    // 默认关闭stdin，全面启用物理文件模式，保障通用引擎跨平台兼容性
    let use_stdin = false; 

    println!("Beginning Test...");
    
    let parsed_count = AtomicUsize::new(0);   
    let passed_count = AtomicUsize::new(0);
    let error_logs = Mutex::new(Vec::new());
    
    let assert_path = Path::new(harness_dir).join("assert.js");
    let sta_path = Path::new(harness_dir).join("sta.js");
    let base_assert = fs::read_to_string(&assert_path).unwrap_or_else(|_| {
        println!("Critical Error: Could not load {:?}", assert_path);
        String::new()
    });
    let base_sta = fs::read_to_string(&sta_path).unwrap_or_else(|_| {
        println!("Critical Error: Could not load {:?}", sta_path);
        String::new()
    });
    //读取所有符合要求的.js文件存入entries
    let entries: Vec<_> = WalkDir::new(test_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && e.path().extension().and_then(|s| s.to_str()) == Some("js"))
        .collect();
    
    let js_file_count = entries.len();
    println!("Found {} JS files, starting parallel execution...", js_file_count);

    //对每个用例读取YAML和JS代码，若YAML解析成功则准备搭建沙箱。
    entries.par_iter().for_each(|entry| {
        let path = entry.path();

        if let Ok(content) = fs::read_to_string(path) {
            if let Some(yaml_str) = extract_frontmatter(&content) {
                if let Ok(rule) = serde_yaml::from_str::<TestRule>(yaml_str) {
                    //针对多线程的情况，这样写确保多个线程同时加1是安全的
                    let current_parsed = parsed_count.fetch_add(1, Ordering::SeqCst) + 1;
                    //检查Test里面有无module块，为下面分类做准备
                    let is_module = rule.flags.as_ref().map_or(false, |f| f.contains(&"module".to_string()));
                    //只保留文件前面的路径。同时，若此文件没有父目录，就使用当前的根目录
                    let parent_dir = path.parent().unwrap_or(Path::new("."));
                    
                    let mut temp_files_to_clean = Vec::new();
                    let output_result = if is_module {
                        //针对ES6的module环境情形
                        //set_up：将全局$262、断言库注入
                        //test：将测试的jsCode注入
                        //entry：提供文件入口
                        let setup_file = format!(".harness_setup_{}.mjs", current_parsed);
                        let test_file = format!(".harness_test_{}.mjs", current_parsed);
                        let entry_file = format!(".harness_entry_{}.mjs", current_parsed);

                        let setup_path = parent_dir.join(&setup_file);
                        let test_path = parent_dir.join(&test_file);
                        let entry_path = parent_dir.join(&entry_file);

                        temp_files_to_clean.push(setup_path.clone());
                        temp_files_to_clean.push(test_path.clone());
                        temp_files_to_clean.push(entry_path.clone());

                        let mut helpers_code = String::new();
                        helpers_code.push_str(&base_assert);
                        helpers_code.push('\n');
                        helpers_code.push_str(&base_sta);
                        helpers_code.push('\n');
                        
                        if let Some(includes) = &rule.includes {
                            for helper_file in includes {
                                let helper_path = Path::new(harness_dir).join(helper_file);
                                if let Ok(helper_content) = fs::read_to_string(&helper_path) {
                                    helpers_code.push_str(&helper_content);
                                    helpers_code.push('\n');
                                }
                            }
                        }
                        
                        // 强行打破环境隔离，将assert.js注入
                        helpers_code.push_str("\nvar _g = typeof globalThis !== 'undefined' ? globalThis : (typeof global !== 'undefined' ? global : this);\n");
                        helpers_code.push_str("if (typeof assert !== 'undefined') _g.assert = assert;\n");
                        helpers_code.push_str("if (typeof Test262Error !== 'undefined') _g.Test262Error = Test262Error;\n");
                        helpers_code.push_str("if (typeof $DONE !== 'undefined') _g.$DONE = $DONE;\n");
                        let escaped_helpers = format!("{:?}", helpers_code);
                        //搭建全局环境沙箱，保证里面的代码在全局作用域执行
                        let setup_content = format!(r#"
                        var _global = typeof globalThis !== 'undefined' ? globalThis : (typeof global !== 'undefined' ? global : (new Function('return this'))());
                        _global.$262 = {{
                            global: _global,
                            evalScript: function(code) {{ return (0, eval)(code); }},
                            createRealm: function() {{ throw new Error('Test262Error: Realm is not natively supported by this engine'); }},
                                detachArrayBuffer: function(buffer) {{
                                if (typeof structuredClone === 'function') structuredClone(buffer, {{transfer: [buffer]}});
                                else throw new Error('Test262Error: detachArrayBuffer is not supported by this engine');
                                }},
                            gc: function() {{}}
                            }};
                            _global.$DONE = function(err) {{
                            if (err) throw new Error('Test262 Async Failed: ' + String(err));
                            }};
                            (0, eval)({});
                        "#, escaped_helpers);

                        fs::write(&setup_path, &setup_content).expect("Failed to write setup file");
                        fs::write(&test_path, extract_js_code(&content)).expect("Failed to write test file");

                        // 确保setup完全执行完毕后再加载并执行 test
                        let entry_content = format!("import \"./{}\";\nimport \"./{}\";\n", setup_file, test_file);
                        fs::write(&entry_path, &entry_content).expect("Failed to write entry file");
                        //执行引擎
                        let mut cmd = Command::new(engine_cmd_name);
                        cmd.current_dir(parent_dir);
                        cmd.arg(&entry_path);
                        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("Failed to start engine").wait_with_output()

                    } else {
                        // 脚本模式
                        let mut final_js = String::new();        
                        // 置顶strict声明
                        if let Some(flags) = &rule.flags {
                            if flags.contains(&"onlyStrict".to_string()) {
                                final_js.push_str("\"use strict\";\n");
                            }
                        }

                        let host_env = r#"
                        var _global = typeof globalThis !== 'undefined' ? globalThis : (typeof global !== 'undefined' ? global : (new Function('return this'))());
                        _global.$262 = {
                        global: _global,
                        evalScript: function(code) { return (0, eval)(code); },
                        createRealm: function() { throw new Error('Test262Error: Realm is not natively supported by this engine'); },
                        detachArrayBuffer: function(buffer) {
                            if (typeof structuredClone === 'function') structuredClone(buffer, {transfer: [buffer]});
                            else throw new Error('Test262Error: detachArrayBuffer is not supported by this engine');
                        },
                        gc: function() {}
                        };
                        _global.$DONE = function(err) {
                        if (err) throw new Error('Test262 Async Failed: ' + String(err));
                        };
                        "#;
                        final_js.push_str(host_env);
                        final_js.push_str(&base_assert);
                        final_js.push('\n');
                        final_js.push_str(&base_sta);
                        final_js.push('\n');
                        
                        //Test262里面有些特定的测试需要其他辅助函数，这里遍历includes下的目录去添加相关辅助代码
                        if let Some(includes) = &rule.includes {
                            for helper_file in includes {
                                let helper_path = Path::new(harness_dir).join(helper_file);
                                if let Ok(helper_content) = fs::read_to_string(&helper_path) {
                                    final_js.push_str(&helper_content);
                                    final_js.push('\n');
                                }
                            }
                        }
                        final_js.push_str(extract_js_code(&content));

                        let test_file = format!(".harness_test_{}.js", current_parsed);
                        let test_path = parent_dir.join(&test_file);
                        temp_files_to_clean.push(test_path.clone());
                        
                        fs::write(&test_path, &final_js).expect("Failed to write test file");

                        let mut cmd = Command::new(engine_cmd_name);
                        cmd.current_dir(parent_dir);

                        if use_stdin {
                            //管道内存直传，是原来为了测试node并提升效率写的，可能就不需要了（默认use_stdin为false）
                            let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("Failed to start engine process");
                            if let Some(mut stdin) = child.stdin.take() {
                                stdin.write_all(final_js.as_bytes()).expect("Failed to write to stdin");
                            }
                            child.wait_with_output()
                        } else {
                            //直接通过物理路径读取
                            cmd.arg(&test_path);
                            cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("Failed to start engine process").wait_with_output()
                        }
                    };
                    
                    //清理临时文件
                    for temp_file in temp_files_to_clean {
                        let _ = fs::remove_file(temp_file);
                    }

                    match output_result {
                        Ok(output) => {
                            let has_crashed = !output.status.success();
                            // 预先提取输出，方便后续多次对比
                            let stderr_str = String::from_utf8_lossy(&output.stderr);
                            let stdout_str = String::from_utf8_lossy(&output.stdout);
                            
                            let test_passed = if let Some(negative) = &rule.negative {
                                // 处理 Negative 类型的测试
                                let expected_type = negative.error_type.as_deref().unwrap_or("");
                                let phase = negative.phase.as_deref().unwrap_or("runtime");
                                let matches_type = expected_type.is_empty() || 
                                                 stderr_str.contains(expected_type) || 
                                                 stdout_str.contains(expected_type);
                                
                                let marker = "Test262: This statement should not be evaluated.";
                                let code_evaluated = stderr_str.contains(marker) || stdout_str.contains(marker);
                                
                                if phase == "parse" || phase == "early" {
                                    // parse/early 阶段必须引擎崩溃、类型匹配、禁止执行后面的代码
                                    has_crashed && matches_type && !code_evaluated
                                } else {
                                    // runtime 阶段必须引擎崩溃、类型匹配
                                    has_crashed && matches_type
                                }
                            } else {
                                // 非Negative情形
                                // 核心修改：不仅不能崩溃，stderr 必须没有内容
                                !has_crashed && stderr_str.trim().is_empty()
                            };

                            if test_passed {
                                println!("Result{}: PASS", current_parsed);
                                passed_count.fetch_add(1, Ordering::SeqCst);
                            } else {
                                let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
                                
                                // 优先抓取 stderr，如果stderr为空但还是失败了，说明是静默失败
                                let mut err_msg = stderr_str.into_owned();
                                if err_msg.trim().is_empty() {
                                    if !stdout_str.trim().is_empty() {
                                        err_msg = stdout_str.into_owned();
                                    } else if has_crashed {
                                        err_msg = "Engine crashed with no output (Exit Code != 0)".to_string();
                                    } else {
                                        err_msg = "Silent Failure: Exit 0 but might have uncaught stderr or failed assertion".to_string();
                                    }
                                }
                                
                                let short_err = err_msg.lines().next().unwrap_or("Unknown error");
                                println!("Result{}: FAIL ({})", current_parsed, short_err);
                                error_logs.lock().unwrap().push((file_name, err_msg));
                            }
                        }
                        Err(_) => println!("Error: Failed to wait on engine process"),
                    }
                }
            }
        }
    });

    let final_parsed = parsed_count.load(Ordering::SeqCst);
    let final_passed = passed_count.load(Ordering::SeqCst);
    let passed_rate : f64 = (final_passed as f64 / final_parsed as f64)*100.0;
    println!("Finished: {} files, {} parsed", js_file_count, final_parsed);
    println!("Passed Tests: {}", final_passed);
    println!("Passed Rate: {:.2} %",passed_rate);

    let logs = error_logs.into_inner().unwrap();
    if !logs.is_empty() {
        //将错误日志导入到一个error_summary.txt的文本文件中
        let log_file_path = "error_summary.txt";
        let mut file = fs::File::create(log_file_path).expect("Failed to create log file");
        for (file_name, err_msg) in logs {
            writeln!(file, "=========================================").unwrap();
            writeln!(file, "FAIL: {}", file_name).unwrap();
            writeln!(file, "-----------------------------------------").unwrap();
            writeln!(file, "{}\n", err_msg.trim()).unwrap();
        }
        println!("All error logs have been saved to {}", log_file_path);
    }
}