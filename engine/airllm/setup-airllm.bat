@echo off
rem One-time setup for AirLLM mode: builds the Python sidecar venv.
rem The venv holds torch CUDA (~3 GB download) + transformers + airllm deps.
rem Default location: D:\Ai Ollama\airllm-venv (change AIRLLM_VENV below if
rem you prefer another drive — then set "airllmPython" in config.json to
rem <venv>\Scripts\python.exe).
setlocal
set "AIRLLM_VENV=D:\Ai Ollama\airllm-venv"
set "PY=C:\Users\ankit\AppData\Local\Programs\Python\Python311\python.exe"

if not exist "%PY%" (
  echo Python 3.11 not found at %PY% — install it first.
  pause & exit /b 1
)

echo [airllm] creating venv: %AIRLLM_VENV%
if not exist "%AIRLLM_VENV%\Scripts\python.exe" "%PY%" -m venv "%AIRLLM_VENV%"

echo [airllm] installing torch CUDA ^(~3 GB, one-time^)...
"%AIRLLM_VENV%\Scripts\python.exe" -m pip install --upgrade pip
"%AIRLLM_VENV%\Scripts\python.exe" -m pip install torch --index-url https://download.pytorch.org/whl/cu124
"%AIRLLM_VENV%\Scripts\python.exe" -m pip install transformers accelerate einops sentencepiece bitsandbytes huggingface-hub

"%AIRLLM_VENV%\Scripts\python.exe" -c "import torch; print('[airllm] torch', torch.__version__, 'cuda', torch.cuda.is_available())"
echo [airllm] done. If config.json has no "airllmPython", the launcher looks in
echo [airllm] engine\airllm\.venv — set "airllmPython": "%AIRLLM_VENV%\Scripts\python.exe".
pause
