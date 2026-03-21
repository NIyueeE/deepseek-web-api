import uvicorn

if __name__ == "__main__":
    uvicorn.run(
        "src.deepseek_web_api:app",
        host="127.0.0.1",
        port=5001,
        reload=True,
    )
