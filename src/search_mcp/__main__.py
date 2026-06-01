from .keystore import load_env_file_into_environ
from .server import run


def main() -> None:
    # Make SEARCH_MCP_* keys in a local .env visible to the keyed engines
    # (keystore reads os.environ, which pydantic's .env loading doesn't populate).
    load_env_file_into_environ()
    run()


if __name__ == "__main__":
    main()
